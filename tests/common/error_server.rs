use crate::common::server::Server;
use async_trait::async_trait;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Response};
use rand::Rng;
use std::sync::{atomic, Arc};
use tokio::sync::oneshot;

// Définit l'état du serveur avec le nombre de requêtes reçues.
#[derive(Debug)]
struct ServerState {
    pub requests_received: atomic::AtomicUsize,
}

// Fonction asynchrone qui retourne une erreur.
#[allow(dead_code)]
async fn return_error() -> Result<Response<Body>, hyper::Error> {
    // Construit et retourne une réponse d'erreur.
    Ok(Response::builder()
        .status(http::StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::empty())
        .unwrap())
}

// Définit le serveur ErrorServer avec ses propriétés.
pub struct ErrorServer {
    shutdown_signal_sender: oneshot::Sender<()>,
    server_task: tokio::task::JoinHandle<()>,
    pub address: String,
    state: Arc<ServerState>,
}

impl ErrorServer {
    // Crée un nouveau ErrorServer sur une adresse IP aléatoire.
    #[allow(dead_code)]
    pub async fn new() -> ErrorServer {
        let mut rng = rand::thread_rng();
        ErrorServer::new_at_address(format!("127.0.0.1:{}", rng.gen_range(1024..65535))).await
    }

    // Crée un nouveau ErrorServer sur une adresse IP spécifiée.
    #[allow(dead_code)]
    pub async fn new_at_address(bind_addr_string: String) -> ErrorServer {
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
                    // Chaque requête reçue incrémentera le compteur et retournera une erreur.
                    Ok::<_, hyper::Error>(service_fn(move |_req| {
                        server_task_state
                            .requests_received
                            .fetch_add(1, atomic::Ordering::SeqCst);
                        return_error()
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
                log::error!("Error in ErrorServer: {}", e);
            }
        });

        // Retourne une instance d'ErrorServer.
        ErrorServer {
            shutdown_signal_sender: shutdown_tx,
            server_task,
            state: server_state,
            address: bind_addr_string,
        }
    }
}

// Implémentation des méthodes spécifiques à ErrorServer.
#[async_trait]
impl Server for ErrorServer {
    async fn stop(self: Box<Self>) -> usize {
        // Envoie un signal pour arrêter le serveur hyper.
        let _ = self.shutdown_signal_sender.send(());
        self.server_task
            .await
            .expect("ErrorServer server task panicked");

        // Retourne le nombre de requêtes reçues.
        self.state.requests_received.load(atomic::Ordering::SeqCst)
    }

    fn address(&self) -> String {
        self.address.clone()
    }
}
