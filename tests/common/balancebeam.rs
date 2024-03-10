use rand::Rng;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::sleep;

// Structure pour gérer les requêtes entre différents serveurs
pub struct BalanceBeam {
    #[allow(dead_code)]
    child: Child, 
    pub address: String,
}

impl BalanceBeam {
    // Détermine le chemin binaire cible pour la balance
    fn target_bin_path() -> std::path::PathBuf {
        let mut path = std::env::current_exe().expect("Could not get current test executable path");
        path.pop();
        path.pop();
        path.push("balancebeam");
        path
    }

    // Crée une nouvelle instance de BalanceBeam
    pub async fn new(
        upstreams: &[&str],
        active_health_check_interval: Option<usize>,
        max_requests_per_minute: Option<usize>,
    ) -> BalanceBeam {
        let mut rng = rand::thread_rng();
        let address = format!("127.0.0.1:{}", rng.gen_range(1024..65535));
        let mut cmd = Command::new(BalanceBeam::target_bin_path());
        cmd.arg("--bind").arg(&address);
        for upstream in upstreams {
            cmd.arg("--upstream").arg(upstream);
        }
        if let Some(active_health_check_interval) = active_health_check_interval {
            cmd.arg("--active-health-check-interval")
                .arg(active_health_check_interval.to_string());
        }
        if let Some(max_requests_per_minute) = max_requests_per_minute {
            cmd.arg("--max-requests-per-minute")
                .arg(max_requests_per_minute.to_string());
        }
        cmd.kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect(&format!(
            "Could not execute balancebeam binary {}",
            BalanceBeam::target_bin_path().to_str().unwrap()
        ));

        // Intercepte et journalise la sortie de la commande
        let stdout = child
            .stdout
            .take()
            .expect("Child process somehow missing stdout pipe!");
        tokio::spawn(async move {
            let mut stdout_reader = BufReader::new(stdout).lines();
            while let Some(line) = stdout_reader
                .next_line()
                .await
                .expect("I/O error reading from child stdout")
            {
                println!("Balancebeam output: {}", line);
            }
        });
        let stderr = child
            .stderr
            .take()
            .expect("Child process somehow missing stderr pipe!");
        tokio::spawn(async move {
            let mut stderr_reader = BufReader::new(stderr).lines();
            while let Some(line) = stderr_reader
                .next_line()
                .await
                .expect("I/O error reading from child stderr")
            {
                println!("Balancebeam output: {}", line);
            }
        });

        // Attend que l'exécutable commence à s'exécuter
        sleep(Duration::from_secs(1)).await;
        BalanceBeam { child, address }
    }

    // Envoie une requête GET à un chemin spécifique
    #[allow(dead_code)]
    pub async fn get(&self, path: &str) -> Result<String, reqwest::Error> {
        let client = reqwest::Client::new();
        client
            .get(&format!("http://{}{}", self.address, path))
            .header("x-sent-by", "balancebeam-tests")
            .send()
            .await?
            .text()
            .await
    }

    // Envoie une requête POST à un chemin spécifique
    #[allow(dead_code)]
    pub async fn post(&self, path: &str, body: &str) -> Result<String, reqwest::Error> {
        let client = reqwest::Client::new();
        client
            .post(&format!("http://{}{}", self.address, path))
            .header("x-sent-by", "balancebeam-tests")
            .body(body.to_string())
            .send()
            .await?
            .text()
            .await
    }
}
