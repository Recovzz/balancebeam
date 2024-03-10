use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_HEADERS_SIZE: usize = 8000;
const MAX_BODY_SIZE: usize = 10000000;
const MAX_NUM_HEADERS: usize = 32;

#[derive(Debug)]
pub enum Error {
    ///Si le serveur a raccroché avant d'envoyer une réponse complète
    IncompleteResponse,
    /// Le serveur a envoyé une réponse HTTP invalide. 
    MalformedResponse(httparse::Error),
    /// Le header Content-Length est présent, mais ne contient pas de valeur numérique valide
    InvalidContentLength,
    /// Le header Content-Length ne correspond pas à la taille du corps de la réponse qui a été envoyée
    ContentLengthMismatch,
    /// Le corps de la réponse est plus grand que MAX_BODY_SIZE
    ResponseBodyTooLarge,
    /// Erreur lors de la lecture/écriture d'un TcpStream
    ConnectionError(std::io::Error),
}

/// Extrait la valeur du header Content-Length de la réponse fournie. Renvoie Ok(Some(usize)) si
/// Content-Length est présent et valide, Ok(None) si Content-Length n'est pas présent, ou
/// Err(Error) si Content-Length est présent mais invalide.
fn get_content_length(response: &http::Response<Vec<u8>>) -> Result<Option<usize>, Error> {
    if let Some(header_value) = response.headers().get("content-length") {
        Ok(Some(
            header_value
                .to_str()
                .or(Err(Error::InvalidContentLength))?
                .parse::<usize>()
                .or(Err(Error::InvalidContentLength))?,
        ))
    } else {
        // Si il n'existe pas retourne None ici
        Ok(None)
    }
}

/// Tente de parser les données du tampon fourni en tant que requête HTTP. Renvoie l'un des
/// suivants :
///
/// * S'il y a une requête complète et valide dans le tampon, renvoie Ok(Some(http::Request))
/// * S'il y a une requête incomplète mais valide jusqu'à présent dans le tampon, renvoie Ok(None)
/// * S'il y a des données dans le tampon qui ne sont définitivement pas une requête HTTP valide, renvoie Err(Error)
fn parse_response(buffer: &[u8]) -> Result<Option<(http::Response<Vec<u8>>, usize)>, Error> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_NUM_HEADERS];
    let mut resp = httparse::Response::new(&mut headers);
    let res = resp
        .parse(buffer)
        .or_else(|err| Err(Error::MalformedResponse(err)))?;

    if let httparse::Status::Complete(len) = res {
        let mut response = http::Response::builder()
            .status(resp.code.unwrap())
            .version(http::Version::HTTP_11);
        for header in resp.headers {
            response = response.header(header.name, header.value);
        }
        let response = response.body(Vec::new()).unwrap();
        Ok(Some((response, len)))
    } else {
        Ok(None)
    }
}

/// Lit une requête HTTP depuis le flux fourni, en attendant qu'un ensemble complet de header soit envoyé.
/// Cette fonction ne lit que la ligne de requête et les headers ; la fonction read_body peut ensuite
/// être appelée pour lire le corps de la requête (pour une requête POST).
///
/// Renvoie Ok(http::Request) si une requête valide est reçue, ou Error sinon.
async fn read_headers(stream: &mut TcpStream) -> Result<http::Response<Vec<u8>>, Error> {
    let mut response_buffer = [0_u8; MAX_HEADERS_SIZE];
    let mut bytes_read = 0;
    loop {
    // Lit des octets depuis la connexion dans le tampon, en commençant à la position bytes_read
        let new_bytes = stream
            .read(&mut response_buffer[bytes_read..])
            .await
            .or_else(|err| Err(Error::ConnectionError(err)))?;
        if new_bytes == 0 {
            return Err(Error::IncompleteResponse);
        }
        bytes_read += new_bytes;

            // renvoie l'erreur lorsque il n'a pas reussi a lire la requete
        if let Some((mut response, headers_len)) = parse_response(&response_buffer[..bytes_read])? {
            response
                .body_mut()
                .extend_from_slice(&response_buffer[headers_len..bytes_read]);
            return Ok(response);
        }
    }
}

/// Cette fonction lit le corps d'une requête à partir du flux. Le client n'envoie un corps que si le
/// Le header Content-Length est présent ; cette fonction lit ce nombre d'octets à partir du flux. Il
/// renvoie Ok(()) si réussi, ou Err(Error) si les octets Content-Length n'ont pas pu être lus.
async fn read_body(
    stream: &mut TcpStream,
    response: &mut http::Response<Vec<u8>>,
) -> Result<(), Error> {
 
    let content_length = get_content_length(response)?;

    while content_length.is_none() || response.body().len() < content_length.unwrap() {
        let mut buffer = [0_u8; 512];
        let bytes_read = stream
            .read(&mut buffer)
            .await
            .or_else(|err| Err(Error::ConnectionError(err)))?;
        if bytes_read == 0 {
            if content_length.is_none() {
                // Lorsue nous atteignons la fin de la reponse
                break;
            } else {
                 // Content-Length a été défini, mais le serveur a raccroché avant que nous ayons réussi à lire ce
                // nombre d'octets
                return Err(Error::ContentLengthMismatch);
            }
        }

        // Assure que le serveur ne nous envoie pas plus d'octets qu'il n'a promis d'envoyer
        if content_length.is_some() && response.body().len() + bytes_read > content_length.unwrap()
        {
            return Err(Error::ContentLengthMismatch);
        }

        // Assure que le serveur n'envoie pas plus d'octets que nous ne permettons
        if response.body().len() + bytes_read > MAX_BODY_SIZE {
            return Err(Error::ResponseBodyTooLarge);
        }

        // Ajoute les octets reçus au corps de la réponse
        response.body_mut().extend_from_slice(&buffer[..bytes_read]);
    }
    Ok(())
}

/// Cette fonction lit et renvoie une réponse HTTP à partir d'un flux, renvoyant une erreur si le serveur
/// ferme la connexion prématurément ou envoie une réponse invalide.
pub async fn read_from_stream(
    stream: &mut TcpStream,
    request_method: &http::Method,
) -> Result<http::Response<Vec<u8>>, Error> {
    let mut response = read_headers(stream).await?;
    // A response may have a body as long as it is not responding to a HEAD request and as long as
    // the response status code is not 1xx, 204 (no content), or 304 (not modified).
    if !(request_method == http::Method::HEAD
        || response.status().as_u16() < 200
        || response.status() == http::StatusCode::NO_CONTENT
        || response.status() == http::StatusCode::NOT_MODIFIED)
    {
        read_body(stream, &mut response).await?;
    }
    Ok(response)
}

///Cette fonction encode une requête en octets et écrit ces octets dans le flux fourni.
pub async fn write_to_stream(
    response: &http::Response<Vec<u8>>,
    stream: &mut TcpStream,
) -> Result<(), std::io::Error> {
    stream
        .write(&format_response_line(response).into_bytes())
        .await?;
    stream.write(&['\r' as u8, '\n' as u8]).await?; // \r\n
    for (header_name, header_value) in response.headers() {
        stream
            .write(&format!("{}: ", header_name).as_bytes())
            .await?;
        stream.write(header_value.as_bytes()).await?;
        stream.write(&['\r' as u8, '\n' as u8]).await?; // \r\n
    }
    stream.write(&['\r' as u8, '\n' as u8]).await?;
    if response.body().len() > 0 {
        stream.write(response.body()).await?;
    }
    Ok(())
}

pub fn format_response_line(response: &http::Response<Vec<u8>>) -> String {
    format!(
        "{:?} {} {}",
        response.version(),
        response.status().as_str(),
        response.status().canonical_reason().unwrap_or("")
    )
}

/// Ceci est une fonction d'aide qui crée une http::Response contenant une erreur HTTP qui peut être
/// envoyé à un client.
pub fn make_http_error(status: http::StatusCode) -> http::Response<Vec<u8>> {
    let body = format!(
        "HTTP {} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    )
    .into_bytes();
    http::Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .header("Content-Length", body.len().to_string())
        .version(http::Version::HTTP_11)
        .body(body)
        .unwrap()
}
