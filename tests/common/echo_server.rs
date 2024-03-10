use crate::common::server::Server;
use async_trait::async_trait;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response};
use rand::Rng;
use std::sync::{atomic, Arc};
use tokio::sync::oneshot;

// Définit l'état du serveur avec le nombre de requêtes reçues.
#[derive(Debug)]
struct ServerState {
    pub requests_received: atomic::AtomicUsize,
}

// Fonction asynchrone qui échoit les requêtes reçues.
async fn echo(
    server_state: Arc<ServerState>,
    req: Request<Body>,
) -> Result<Response<Body>, hyper::Error> {
    // Incrémente le nombre de requêtes reçues.
    server_state
        .requests_received
        .fetch_add(1, atomic::Ordering::SeqCst);
    // Construit le texte de la requête pour l'écho.
    let mut req_text = format!("{} {} {:?}\n", req.method(), req.uri(), req.version());
    for (header_name, header_value) in req.headers() {
        req_text += &format!(
            "{}: {}\n",
            header_name.as_str(),
            header_value.to_str().unwrap_or("<binary value>")
        );
    }
    req_text += "\n";
    // Convertit la requête en bytes pour la réponse.
    let mut req_as_bytes = req_text.into_bytes();
    req_as_bytes.extend(hyper::body::to_bytes(req.into_body()).await?);
    // Retourne la réponse avec le contenu de la requête.
    Ok(Response::new(Body::from(req_as_bytes)))
}

// Définit le serveur Echo avec ses propriétés.
pub struct EchoServer {
    shutdown_signal_sender: oneshot::Sender<()>,
    server_task: tokio::task::JoinHandle<()>,
    pub address: String,
    state: Arc<ServerState>,
}

impl EchoServer {
    // Crée un nouveau serveur Echo sur une adresse IP aléatoire.
    pub async fn new() -> EchoServer {
        let mut rng = rand::thread_rng();
        EchoServer::new_at_address(format!("127.0.0.1:{}", rng.gen_range(1024..65535))).await
    }

    // Crée un nouveau serveur Echo sur une adresse IP spécifiée.
    pub async fn new_at_address(bind_addr_string: String) -> EchoServer {
        let bind_addr = bind_addr_string.parse().unwrap();
        // Crée un canal one-shot pour signaler l'arrêt du serveur.
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Démarre une tâche serveur séparée.
        let server_state = Arc::new(ServerState {
            requests_received: atomic::AtomicUsize::new(0),
        });
        let server_task_state = server_state.clone();
        let server_task = tokio::spawn(async move {
            let service = make_service_fn(|_| {
                let server_task_state = server_task_state.clone();
                async move {
                    Ok::<_, hyper::Error>(service_fn(move |req| {
                        let server_task_state = server_task_state.clone();
                        echo(server_task_state, req)
                    }))
                }
            });
            let server = hyper::Server::bind(&bind_addr)
                .serve(service)
                .with_graceful_shutdown(async {
                    // Attente de l'ordre d'arrêt.
                    shutdown_rx.await.ok();
                });
            // Démarrage et gestion des erreurs du serveur.
            if let Err(e) = server.await {
                log::error!("Error in EchoServer: {}", e);
            }
        });

        // Retourne une instance du serveur Echo.
        EchoServer {
            shutdown_signal_sender: shutdown_tx,
            server_task,
            state: server_state,
            address: bind_addr_string,
        }
    }
}

// Implémentation des méthodes spécifiques au serveur Echo.
#[async_trait]
impl Server for EchoServer {
    // Arrête le serveur Echo.
    async fn stop(self: Box<Self>) -> usize {
        // Envoie un signal pour arrêter le serveur hyper.
        let _ = self.shutdown_signal_sender.send(());
        // Attente de l'arrêt du serveur.
        self.server_task
            .await
            .expect("ErrorServer server task panicked");

        // Retourne le nombre de requêtes reçues.
        self.state.requests_received.load(atomic::Ordering::SeqCst)
    }

    // Retourne l'adresse du serveur.
    fn address(&self) -> String {
        self.address.clone()
    }
}
