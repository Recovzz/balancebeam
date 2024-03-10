use std::cmp::min;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_HEADERS_SIZE: usize = 8000;
const MAX_BODY_SIZE: usize = 10000000;
const MAX_NUM_HEADERS: usize = 32;

#[derive(Debug)]
pub enum Error {
    /// Le client a raccroché avant d'envoyer une requête complète. IncompleteRequest contient le nombre de
    /// octets qui ont été lus avec succès avant que le client ne raccroche
    IncompleteRequest(usize),
     /// Le client a envoyé une requête HTTP invalide. httparse::Error contient plus de détails
    MalformedRequest(httparse::Error),
    /// Le header Content-Length est présent, mais ne contient pas de valeur numérique valide
    InvalidContentLength,
    /// Le Header Content-Length ne correspond pas à la taille du corps de la requête qui a été envoyée
    ContentLengthMismatch,
    /// Le corps de la requête est plus grand que MAX_BODY_SIZE
    RequestBodyTooLarge,
    /// Erreur lors de la lecture/écriture d'un TcpStream
    ConnectionError(std::io::Error),
}

///  Extrait la valeur de le header Content-Length de la requête fournie. Renvoie Ok(Some(usize)) si
/// Content-Length est présent et valide, Ok(None) si Content-Length n'est pas présent, ou
/// Err(Error) si Content-Length est présent mais invalide.
fn get_content_length(request: &http::Request<Vec<u8>>) -> Result<Option<usize>, Error> {
    // Recherche du header content-length
    if let Some(header_value) = request.headers().get("content-length") {
        Ok(Some(
            header_value
                .to_str()
                .or(Err(Error::InvalidContentLength))?
                .parse::<usize>()
                .or(Err(Error::InvalidContentLength))?,
        ))
    } else {
        // Si il n'existe pas i lrenvoie le retour None
        Ok(None)
    }
}

/// Cette fonction ajoute une valeur à un header (ajoutant un nouvel header si le header n'est pas déjà
/// présent). Cela est utilisé pour ajouter l'adresse IP du client à la fin de la liste X-Forwarded-For,
/// ou pour ajouter un nouvel header X-Forwarded-For s'il n'est pas déjà présent.
pub fn extend_header_value(
    request: &mut http::Request<Vec<u8>>,
    name: &'static str,
    extend_value: &str,
) {
    let new_value = match request.headers().get(name) {
        Some(existing_value) => {
            [existing_value.as_bytes(), b", ", extend_value.as_bytes()].concat()
        }
        None => extend_value.as_bytes().to_owned(),
    };
    request
        .headers_mut()
        .insert(name, http::HeaderValue::from_bytes(&new_value).unwrap());
}

/// Tente de parser les données du tampon fourni en tant que requête HTTP. Renvoie l'un des
/// suivants :
///
/// * S'il y a une requête complète et valide dans le tampon, renvoie Ok(Some(http::Request))
/// * S'il y a une requête incomplète mais valide jusqu'à présent dans le tampon, renvoie Ok(None)
/// * S'il y a des données dans le tampon qui ne sont définitivement pas une requête HTTP valide, renvoie Err(Error)
fn parse_request(buffer: &[u8]) -> Result<Option<(http::Request<Vec<u8>>, usize)>, Error> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_NUM_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    let res = req
        .parse(buffer)
        .or_else(|err| Err(Error::MalformedRequest(err)))?;

    if let httparse::Status::Complete(len) = res {
        let mut request = http::Request::builder()
            .method(req.method.unwrap())
            .uri(req.path.unwrap())
            .version(http::Version::HTTP_11);
        for header in req.headers {
            request = request.header(header.name, header.value);
        }
        let request = request.body(Vec::new()).unwrap();
        Ok(Some((request, len)))
    } else {
        Ok(None)
    }
}

/// Lit une requête HTTP depuis le flux fourni, en attendant qu'un ensemble complet de header soit envoyé.
/// Cette fonction ne lit que la ligne de requête et les headers ; la fonction read_body peut ensuite
/// être appelée pour lire le corps de la requête (pour une requête POST).
///
/// Renvoie Ok(http::Request) si une requête valide est reçue, ou Error sinon.

async fn read_headers(stream: &mut TcpStream) -> Result<http::Request<Vec<u8>>, Error> {
    let mut request_buffer = [0_u8; MAX_HEADERS_SIZE];
    let mut bytes_read = 0;
    loop {
        // Lit des octets depuis la connexion dans le tampon, en commençant à la position bytes_read
        let new_bytes = stream
            .read(&mut request_buffer[bytes_read..])
            .await
            .or_else(|err| Err(Error::ConnectionError(err)))?;
        if new_bytes == 0 {
            // renvoie l'erreur lorsque il n'a pas reussi a lire la requete
            return Err(Error::IncompleteRequest(bytes_read));
        }
        bytes_read += new_bytes;

        // Ici une verification est faite si nous avons lu une requete valide jusqu'a maintenant.
        if let Some((mut request, headers_len)) = parse_request(&request_buffer[..bytes_read])? {
       
            request
                .body_mut()
                .extend_from_slice(&request_buffer[headers_len..bytes_read]);
            return Ok(request);
        }
    }
}

/// Cette fonction lit le corps d'une requête à partir du flux. Le client n'envoie un corps que si le
/// Le header Content-Length est présent ; cette fonction lit ce nombre d'octets à partir du flux. Il
/// renvoie Ok(()) si réussi, ou Err(Error) si les octets Content-Length n'ont pas pu être lus.
async fn read_body(
    stream: &mut TcpStream,
    request: &mut http::Request<Vec<u8>>,
    content_length: usize,
) -> Result<(), Error> {
    // Continue de lire des données jusqu'à ce que nous lisions la longueur totale du corps, ou jusqu'à ce que nous rencontrions une erreur.
    while request.body().len() < content_length {
      // Lire jusqu'à 512 octets à la fois.
        let mut buffer = vec![0_u8; min(512, content_length)];
        let bytes_read = stream
            .read(&mut buffer)
            .await
            .or_else(|err| Err(Error::ConnectionError(err)))?;

        // Assure que le client envoie toujours des octets
        if bytes_read == 0 {
            log::debug!(
                "Client hung up after sending a body of length {}, even though it said the content \
                length is {}",
                request.body().len(),
                content_length
            );
            return Err(Error::ContentLengthMismatch);
        }

        // Assure que le client ne nous envoie pas *trop* d'octets
        if request.body().len() + bytes_read > content_length {
            log::debug!(
                "Client sent more bytes than we expected based on the given content length!"
            );
            return Err(Error::ContentLengthMismatch);
        }
        // stock les octets reçu
        request.body_mut().extend_from_slice(&buffer[..bytes_read]);
    }
    Ok(())
}

/// Cette fonction lit et renvoie une requête HTTP à partir d'un flux, renvoyant une erreur si le client
/// ferme la connexion prématurément ou envoie une requête invalide.
pub async fn read_from_stream(stream: &mut TcpStream) -> Result<http::Request<Vec<u8>>, Error> {
    // lit les headers
    let mut request = read_headers(stream).await?;
    if let Some(content_length) = get_content_length(&request)? {
        if content_length > MAX_BODY_SIZE {
            return Err(Error::RequestBodyTooLarge);
        } else {
            read_body(stream, &mut request, content_length).await?;
        }
    }
    Ok(request)
}

/// Cette fonction encode une requête en octets et écrit ces octets dans le flux fourni.
pub async fn write_to_stream(
    request: &http::Request<Vec<u8>>,
    stream: &mut TcpStream,
) -> Result<(), std::io::Error> {
    stream
        .write(&format_request_line(request).into_bytes())
        .await?;
    stream.write(&['\r' as u8, '\n' as u8]).await?; // \r\n
    for (header_name, header_value) in request.headers() {
        stream
            .write(&format!("{}: ", header_name).as_bytes())
            .await?;
        stream.write(header_value.as_bytes()).await?;
        stream.write(&['\r' as u8, '\n' as u8]).await?; // \r\n
    }
    stream.write(&['\r' as u8, '\n' as u8]).await?;
    if request.body().len() > 0 {
        stream.write(request.body()).await?;
    }
    Ok(())
}

pub fn format_request_line(request: &http::Request<Vec<u8>>) -> String {
    format!(
        "{} {} {:?}",
        request.method(),
        request.uri(),
        request.version()
    )
}
