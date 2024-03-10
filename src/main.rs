mod request;
mod response;

use clap::Parser;

use rand::{Rng, SeedableRng};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;

/// Contient les informations analysées a partir de l'invocation en ligne de commande de balancebeam.
#[derive(Parser, Debug)]
#[command(about = "Fun with load balancing")]
struct CmdOptions {
    /// "IP/port a lier"
    #[arg(short, long, default_value = "0.0.0.0:1100")]
    bind: String,
    /// "Upstream vers lequel transferer les requetes"
    #[arg(short, long)]
    upstream: Vec<String>,
    /// "Effectuer des health checks actives sur cet intervalle"
    #[arg(long, default_value = "10")]
    active_health_check_interval: usize,
    /// "Chemin vers lequel envoyer les requetes pour les verifications de health checks"
    #[arg(long, default_value = "/")]
    active_health_check_path: String,
    /// "Nombre maximum de requetes acceptees par IP par minute"
    #[arg(long, default_value = "0")]
    max_requests_per_minute: usize,
}

/// La partie suivante contient des informations sur l'etat de balancebeam (par exemple, vers quels serveurs nous redirigeons actuellement, quels serveurs ont echoué, etc.)

#[derive(Clone)]
struct ProxyState {
    active_health_check_interval: usize,
    active_health_check_path: String,
    max_requests_per_minute: usize,
    upstream_addresses: Vec<String>,
    live_upstream_addresses: Arc<RwLock<Vec<String>>>,
    rate_limiting_counter: Arc<Mutex<HashMap<String, usize>>>,
}

#[tokio::main]
async fn main() {
    // Initialise la librairie. 
    if let Err(_) = std::env::var("RUST_LOG") {
        std::env::set_var("RUST_LOG", "debug");
    }
    pretty_env_logger::init();

    // Analyse des arguments de ligne de commande passes a ce programm
    let options = CmdOptions::parse();
    if options.upstream.len() < 1 {
        log::error!("At least one upstream server must be specified using the --upstream option.");
        std::process::exit(1);
    }

    // Commence à écouter les connexions
    let listener = match TcpListener::bind(&options.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("Could not bind to {}: {}", options.bind, err);
            std::process::exit(1);
        }
    };
    log::info!("Listening for requests on {}", options.bind);

    let state = ProxyState {
        live_upstream_addresses: Arc::new(RwLock::new(options.upstream.clone())),
        upstream_addresses: options.upstream,
        active_health_check_interval: options.active_health_check_interval,
        active_health_check_path: options.active_health_check_path,
        max_requests_per_minute: options.max_requests_per_minute,
        rate_limiting_counter: Arc::new(Mutex::new(HashMap::new())),
    };

    // Commence le health check actif
    let state_temp = state.clone();
    tokio::spawn(async move {
        active_health_check(&state_temp).await;
    });

    // Commence a nettoyer le compteur de limitation de debit toutes les minutes
    let state_temp = state.clone();
    tokio::spawn(async move {
        rate_limiting_counter_clearer(&state_temp, 60).await;
    });

    // Gere les connexions entrantes
    loop {
        if let Ok((stream, _)) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                handle_connection(stream, &state).await;
            });
        }
    }
}

async fn rate_limiting_counter_clearer(state: &ProxyState, clear_interval: u64) {
    loop {
        sleep(Duration::from_secs(clear_interval)).await;
        let mut rate_limiting_counter = state.rate_limiting_counter.clone().lock_owned().await;
        rate_limiting_counter.clear();
    }
}

async fn active_health_check(state: &ProxyState) {
    loop {
        sleep(Duration::from_secs(
            state.active_health_check_interval.try_into().unwrap(),
        ))
        .await;

        let mut live_upstream_addresses = state.live_upstream_addresses.write().await;
        live_upstream_addresses.clear();
        // Envoie une requete a tout les upstream.
        for upstream_ip in &state.upstream_addresses {
            let request = http::Request::builder()
                .method(http::Method::GET)
                .uri(&state.active_health_check_path)
                .header("Host", upstream_ip)
                .body(Vec::new())
                .unwrap();
            // Ouvre une connexion vers un serveur de destination
            match TcpStream::connect(upstream_ip).await {
                Ok(mut conn) => {
                    if let Err(error) = request::write_to_stream(&request, &mut conn).await {
                        log::error!(
                            "Failed to send request to upstream {}: {}",
                            upstream_ip,
                            error
                        );
                        return;
                    }
                    let response =
                        match response::read_from_stream(&mut conn, &request.method()).await {
                            Ok(response) => response,
                            Err(error) => {
                                log::error!("Error reading response from server: {:?}", error);
                                return;
                            }
                        };
                    // Gerer le code d'etat de la reponse
                    match response.status().as_u16() {
                        200 => {
                            live_upstream_addresses.push(upstream_ip.clone());
                        }
                        status @ _ => {
                            log::error!(
                                "upstream server {} is not working: {}",
                                upstream_ip,
                                status
                            );
                            return;
                        }
                    }
                }
                Err(err) => {
                    log::error!("Failed to connect to upstream {}: {}", upstream_ip, err);
                    return;
                }
            }
        }
    }
}

async fn connect_to_upstream(state: &ProxyState) -> Result<TcpStream, std::io::Error> {
    let mut rng = rand::rngs::StdRng::from_entropy();
    loop {
        let live_upstream_addresses = state.live_upstream_addresses.read().await;
        let upstream_idx = rng.gen_range(0..live_upstream_addresses.len());
        let upstream_ip = &live_upstream_addresses.get(upstream_idx).unwrap().clone();
        drop(live_upstream_addresses); // libere le verrou en lecture

        match TcpStream::connect(upstream_ip).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                // gerer les upstream addresses mortes
                log::error!("Failed to connect to upstream {}: {}", upstream_ip, err);
                let mut live_upstream_addresses = state.live_upstream_addresses.write().await;
                live_upstream_addresses.swap_remove(upstream_idx); // les supprimes ici

                // Et retournne une erreur quand elles sont toutes mortes 
                if live_upstream_addresses.len() == 0 {
                    log::error!("All upstreams are dead");
                    return Err(Error::new(ErrorKind::Other, "All upstreams are dead"));
                }
            }
        }
    }
}

async fn send_response(client_conn: &mut TcpStream, response: &http::Response<Vec<u8>>) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!(
        "{} <- {}",
        client_ip,
        response::format_response_line(&response)
    );
    if let Err(error) = response::write_to_stream(&response, client_conn).await {
        log::warn!("Failed to send response to client: {}", error);
        return;
    }
}

async fn check_rate(state: &ProxyState, client_conn: &mut TcpStream) -> Result<(), std::io::Error> {
    // Recupere l'adresse IP du client a partir de la connexion TCP
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    let mut rate_limiting_counter = state.rate_limiting_counter.clone().lock_owned().await;
    let cnt = rate_limiting_counter.entry(client_ip).or_insert(0);
    *cnt += 1;

    if *cnt > state.max_requests_per_minute {
        let response = response::make_http_error(http::StatusCode::TOO_MANY_REQUESTS);
        if let Err(error) = response::write_to_stream(&response, client_conn).await {
            log::warn!("Failed to send response to client: {}", error);
        }
        return Err(Error::new(ErrorKind::Other, "Rate limiting"));
    }
    Ok(())
}

async fn handle_connection(mut client_conn: TcpStream, state: &ProxyState) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!("Connection received from {}", client_ip);

    // Ouvre une connexion vers un serveur de destination aleatoire
    let mut upstream_conn = match connect_to_upstream(state).await {
        Ok(stream) => stream,
        Err(_error) => {
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response).await;
            return;
        }
    };
    let upstream_ip = upstream_conn.peer_addr().unwrap().ip().to_string();

    // Le client peut maintenant nous envoyer une ou plusieurs requetes. 
    loop {
        // Lit une requete du client
        let mut request = match request::read_from_stream(&mut client_conn).await {
            Ok(request) => request,
            // Gere ici le cas ou le client a ferme la connexion et ne transmet plus de requetes
            Err(request::Error::IncompleteRequest(0)) => {
                log::debug!("Client finished sending requests. Shutting down connection");
                return;
            }
            // Gestion des errerus lors de la lecture depuis le client
            Err(request::Error::ConnectionError(io_err)) => {
                log::info!("Error reading request from client stream: {}", io_err);
                return;
            }
            Err(error) => {
                log::debug!("Error parsing request: {:?}", error);
                let response = response::make_http_error(match error {
                    request::Error::IncompleteRequest(_)
                    | request::Error::MalformedRequest(_)
                    | request::Error::InvalidContentLength
                    | request::Error::ContentLengthMismatch => http::StatusCode::BAD_REQUEST,
                    request::Error::RequestBodyTooLarge => http::StatusCode::PAYLOAD_TOO_LARGE,
                    request::Error::ConnectionError(_) => http::StatusCode::SERVICE_UNAVAILABLE,
                });
                send_response(&mut client_conn, &response).await;
                continue;
            }
        };
        log::info!(
            "{} -> {}: {}",
            client_ip,
            upstream_ip,
            request::format_request_line(&request)
        );

        // limitaion du debit ici
        if state.max_requests_per_minute > 0 {
            if let Err(_error) = check_rate(&state, &mut client_conn).await {
                log::error!("{} rate limiting", &client_ip);
                continue;
            }
        }

        //  Ajoute l'en-tete X-Forwarded-For pour que l'upstream connaisse l'adresse IP du client.
        request::extend_header_value(&mut request, "x-forwarded-for", &client_ip);

        // Transfere la demande au serveur
        if let Err(error) = request::write_to_stream(&request, &mut upstream_conn).await {
            log::error!(
                "Failed to send request to upstream {}: {}",
                upstream_ip,
                error
            );
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response).await;
            return;
        }
        log::debug!("Forwarded request to server");

        // lectrure de la reponse du serveur ici 
        let response = match response::read_from_stream(&mut upstream_conn, request.method()).await
        {
            Ok(response) => response,
            Err(error) => {
                log::error!("Error reading response from server: {:?}", error);
                let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
                send_response(&mut client_conn, &response).await;
                return;
            }
        };
        // trasnfere de la reponse du client
        send_response(&mut client_conn, &response).await;
        log::debug!("Forwarded response to client");
    }
}
