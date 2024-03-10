mod common;

use common::{init_logging, BalanceBeam, EchoServer, ErrorServer, Server};

use std::time::Duration;
use tokio::time::sleep;

// Fonction asynchrone pour configurer l'environnement avec des paramètres spécifiques.
// Crée un certain nombre de serveurs amont et configure un balanceur de charge avec eux.
async fn setup_with_params(
    n_upstreams: usize,
    active_health_check_interval: Option<usize>,
    max_requests_per_minute: Option<usize>,
) -> (BalanceBeam, Vec<Box<dyn Server>>) {
    // Initialisation du système de journalisation.
    init_logging();
    // Création des serveurs amont.
    let mut upstreams: Vec<Box<dyn Server>> = Vec::new();
    for _ in 0..n_upstreams {
        upstreams.push(Box::new(EchoServer::new().await));
    }
    // Récupération des adresses des serveurs amont.
    let upstream_addresses: Vec<String> = upstreams
        .iter()
        .map(|upstream| upstream.address())
        .collect();
    let upstream_addresses: Vec<&str> = upstream_addresses
        .iter()
        .map(|addr| addr.as_str())
        .collect();
    // Création du balanceur de charge.
    let balancebeam = BalanceBeam::new(
        &upstream_addresses,
        active_health_check_interval,
        max_requests_per_minute,
    )
    .await;
    (balancebeam, upstreams)
}

// Fonction de configuration simplifiée sans paramètres spécifiques.
async fn setup(n_upstreams: usize) -> (BalanceBeam, Vec<Box<dyn Server>>) {
    setup_with_params(n_upstreams, None, None).await
}

// Test asynchrone pour vérifier la distribution équitable des requêtes entre les serveurs amont.
#[tokio::test]
async fn test_load_distribution() {
    // Configuration initiale.
    let n_upstreams = 3;
    let n_requests = 90;
    let (balancebeam, mut upstreams) = setup(n_upstreams).await;

    // Envoi des requêtes au balanceur de charge.
    for i in 0..n_requests {
        let path = format!("/request-{}", i);
        let response_text = balancebeam
            .get(&path)
            .await
            .expect("Error sending request to balancebeam");
        // Vérifications sur la réponse.
        assert!(response_text.contains(&format!("GET {} HTTP/1.1", path)));
        assert!(response_text.contains("x-sent-by: balancebeam-tests"));
        assert!(response_text.contains("x-forwarded-for: 127.0.0.1"));
    }

    // Comptage des requêtes reçues par chaque serveur amont.
    let mut request_counters = Vec::new();
    while let Some(upstream) = upstreams.pop() {
        request_counters.insert(0, upstream.stop().await);
    }
    // Vérification de la distribution des requêtes.
    log::info!(
        "Number of requests received by each upstream: {:?}",
        request_counters
    );
    let avg_req_count =
        request_counters.iter().sum::<usize>() as f64 / request_counters.len() as f64;
    log::info!("Average number of requests per upstream: {}", avg_req_count);
    for upstream_req_count in request_counters {
        if (upstream_req_count as f64 - avg_req_count).abs() > 0.4 * avg_req_count {
            log::error!(
                "Upstream request count {} differs too much from the average! Load doesn't seem \
                evenly distributed.",
                upstream_req_count
            );
            panic!("Upstream request count differs too much");
        }
    }

    log::info!("All done :)");
}

// Fonction asynchrone pour tester le basculement en cas de défaillance d'un serveur amont.
async fn try_failover(balancebeam: &BalanceBeam, upstreams: &mut Vec<Box<dyn Server>>) {
    // Envoi de quelques requêtes initiales.
    log::info!("Sending some initial requests. These should definitely work.");
    for i in 0..5 {
        let path = format!("/request-{}", i);
        let response_text = balancebeam
            .get(&path)
            .await
            .expect("Error sending request to balancebeam");
        // Vérifications sur la réponse.
        assert!(response_text.contains(&format!("GET {} HTTP/1.1", path)));
        assert!(response_text.contains("x-sent-by: balancebeam-tests"));
        assert!(response_text.contains("x-forwarded-for: 127.0.0.1"));
    }

    // Arrêt d'un des serveurs amont.
    log::info!("Killing one of the upstream servers");
    upstreams.pop().unwrap().stop().await;

    // Vérification que les requêtes continuent de fonctionner malgré la défaillance.
    for i in 0..6 {
        log::info!("Sending request #{} after killing an upstream server", i);
        let path = format!("/failover-{}", i);
        let response_text = balancebeam
            .get(&path)
            .await
            .expect("Error sending request to balancebeam. Passive failover may not be working");
        // Vérifications sur la réponse.
        assert!(
            response_text.contains(&format!("GET {} HTTP/1.1", path)),
            "balancebeam returned unexpected response. Failover may not be working."
        );
        assert!(
            response_text.contains("x-sent-by: balancebeam-tests"),
            "balancebeam returned unexpected response. Failover may not be working."
        );
        assert!(
            response_text.contains("x-forwarded-for: 127.0.0.1"),
            "balancebeam returned unexpected response. Failover may not be working."
        );
    }
}

// Test asynchrone pour vérifier le fonctionnement des vérifications de santé passives.
// Assurez-vous que les requêtes continuent de fonctionner après la défaillance d'un serveur amont.
#[tokio::test]
async fn test_passive_health_checks() {
    // Configuration initiale.
    let n_upstreams = 2;
    let (balancebeam, mut upstreams) = setup(n_upstreams).await;
    try_failover(&balancebeam, &mut upstreams).await;
    log::info!("All done :)");
}

// Test asynchrone pour vérifier que les contrôles de santé actifs surveillent le statut HTTP.
// Assurez-vous que le balanceur de charge ne se fie pas uniquement à la possibilité d'établir des connexions.
#[tokio::test]
async fn test_active_health_checks_check_http_status() {
    // Configuration initiale avec vérification de santé active.
    let n_upstreams = 2;
    let (balancebeam, mut upstreams) = setup_with_params(n_upstreams, Some(1), None).await;
    let failed_ip = upstreams[upstreams.len() - 1].address();

    // Envoi de quelques requêtes initiales.
    log::info!("Sending some initial requests. These should definitely work.");
    for i in 0..4 {
        let path = format!("/request-{}", i);
        let response_text = balancebeam
            .get(&path)
            .await
            .expect("Error sending request to balancebeam");
        // Vérifications sur la réponse.
        assert!(response_text.contains(&format!("GET {} HTTP/1.1", path)));
        assert!(response_text.contains("x-sent-by: balancebeam-tests"));
        assert!(response_text.contains("x-forwarded-for: 127.0.0.1"));
    }

    // Remplacement d'un serveur amont par un serveur renvoyant des erreurs HTTP 500.
    log::info!("Replacing one of the upstreams with a server that returns Error 500s...");
    upstreams.pop().unwrap().stop().await;
    upstreams.push(Box::new(ErrorServer::new_at_address(failed_ip).await));

    // Attente pour que les vérifications de santé détectent le serveur défaillant.
    log::info!("Waiting for health checks to realize server is dead...");
    sleep(Duration::from_secs(3)).await;

    // Envoi de requêtes supplémentaires et vérification de leur réussite.
    for i in 0..8 {
        log::info!(
            "Sending request #{} after swapping server for one that returns Error 500. We should \
            get a successful response from the other upstream",
            i
        );
        let path = format!("/failover-{}", i);
        let response_text = balancebeam.get(&path).await.expect(
            "Error sending request to balancebeam. Active health checks may not be working",
        );
        // Vérifications sur la réponse.
        assert!(
            response_text.contains(&format!("GET {} HTTP/1.1", path)),
            "balancebeam returned unexpected response. Active health checks may not be working."
        );
        assert!(
            response_text.contains("x-sent-by: balancebeam-tests"),
            "balancebeam returned unexpected response. Active health checks may not be working."
        );
        assert!(
            response_text.contains("x-forwarded-for: 127.0.0.1"),
            "balancebeam returned unexpected response. Active health checks may not be working."
        );
    }
}

// Test asynchrone pour vérifier que les contrôles de santé actifs rétablissent les serveurs amont précédemment en échec.
#[tokio::test]
async fn test_active_health_checks_restore_failed_upstream() {
    // Configuration initiale avec vérification de santé active.
    let n_upstreams = 2;
    let (balancebeam, mut upstreams) = setup_with_params(n_upstreams, Some(1), None).await;
    let failed_ip = upstreams[upstreams.len() - 1].address();
    try_failover(&balancebeam, &mut upstreams).await;

    // Redémarrage d'un serveur amont précédemment en échec.
    log::info!("Re-starting the \"failed\" upstream server...");
    upstreams.push(Box::new(EchoServer::new_at_address(failed_ip).await));

    // Attente pour que la vérification de santé active détecte le serveur rétabli.
    log::info!("Waiting a few seconds for the active health check to run...");
    sleep(Duration::from_secs(3)).await;

    // Envoi de requêtes supplémentaires pour vérifier le rétablissement.
    log::info!("Sending some more requests");
    for i in 0..5 {
        let path = format!("/after-restore-{}", i);
        let response_text = balancebeam
            .get(&path)
            .await
            .expect("Error sending request to balancebeam");
        // Vérifications sur la réponse.
        assert!(response_text.contains(&format!("GET {} HTTP/1.1", path)));
        assert!(response_text.contains("x-sent-by: balancebeam-tests"));
        assert!(response_text.contains("x-forwarded-for: 127.0.0.1"));
    }

    // Vérification que le serveur amont rétabli a reçu des requêtes.
    log::info!(
        "Verifying that the previously-dead upstream got some requests after being restored"
    );
    let last_upstream_req_count = upstreams.pop().unwrap().stop().await;
    assert!(
        last_upstream_req_count > 0,
        "We killed an upstream, then brought it back, but it never got any more requests!"
    );

    // Fermeture des serveurs amont.
    while let Some(upstream) = upstreams.pop() {
        upstream.stop().await;
    }

    log::info!("All done :)");
}

// Test asynchrone pour vérifier le fonctionnement de la limitation du taux de requêtes.
#[tokio::test]
async fn test_rate_limiting() {
    // Configuration initiale avec limitation du taux de requêtes.
    let n_upstreams = 1;
    let rate_limit_threshold = 5;
    let num_extra_requests: usize = 3;
    let (balancebeam, mut upstreams) =
        setup_with_params(n_upstreams, None, Some(rate_limit_threshold)).await;

    // Envoi de requêtes dans la limite du taux autorisé.
    log::info!(
        "Sending some basic requests to the server, within the rate limit threshold. These \
        should succeed."
    );
    for i in 0..rate_limit_threshold {
        let path = format!("/request-{}", i);
        let response_text = balancebeam
            .get(&path)
            .await
            .expect("Error sending request to balancebeam");
        // Vérifications sur la réponse.
        assert!(response_text.contains(&format!("GET {} HTTP/1.1", path)));
        assert!(response_text.contains("x-sent-by: balancebeam-tests"));
        assert!(response_text.contains("x-forwarded-for: 127.0.0.1"));
    }

    // Envoi de requêtes dépassant la limite du taux autorisé.
    log::info!(
        "Sending more requests that exceed the rate limit threshold. The server should \
        respond to these with an HTTP 429 (too many requests) error."
    );
    for i in 0..num_extra_requests {
        let client = reqwest::Client::new();
        let response = client
            .get(&format!("http://{}/overboard-{}", balancebeam.address, i))
            .header("x-sent-by", "balancebeam-tests")
            .send()
            .await
            .expect(
                "Error sending rate limited request to balancebeam. You should be \
                accepting the connection but sending back an HTTP error, rather than rejecting \
                the connection outright.",
            );
        // Vérifications sur la réponse HTTP 429.
        log::info!("{:?}", response);
        log::info!("Checking to make sure the server responded with HTTP 429");
        assert_eq!(response.status().as_u16(), 429);
    }

    // Vérification que les requêtes supplémentaires n'ont pas été traitées par les serveurs amont.
    log::info!("Ensuring the extra requests didn't go through to the upstream servers");
    let mut total_request_count = 0;
    while let Some(upstream) = upstreams.pop() {
        total_request_count += upstream.stop().await;
    }
    assert_eq!(total_request_count, rate_limit_threshold);

    log::info!("All done :)");
}
