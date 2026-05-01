use anyhow::{Context, Result, anyhow, bail};
use lambda_http::{service_fn, Error, lambda_runtime::{self, LambdaEvent}, tracing::{self, Level}};
use reqwest;
use serde::Serialize;
use serde_json::Value;
use std::{env, time::Duration};
use url::Url;

#[derive(Serialize)]
struct ResponseData {
    event: EventData,
}

#[derive(Serialize)]
struct EventData {
    payload: PayloadData,
}

#[derive(Serialize)]
struct PayloadData {
    #[serde(rename = "type")]
    t: String,
    message: String,
}

struct Timer(Option<std::time::Instant>);
impl Timer {
    fn start() -> Timer {
        Timer(if tracing::enabled!(Level::DEBUG) { Some(std::time::Instant::now()) } else { None })
    }
    fn end(self, msg: &str) {
        if let Some(start) = self.0 {
            tracing::debug!("{} in {:.2?}", msg, start.elapsed());
        }
    }
}

fn parse_custom_headers() -> Result<Vec<(String, String)>> {
    let headers_str = match env::var("CUSTOM_HEADERS") {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    let mut headers = Vec::new();
    for part in headers_str.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = part.split_once(':')
            .ok_or_else(|| anyhow!("Invalid custom header format: '{}'. Expected 'Name: Value'", part))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(headers)
}

async fn lookup_url() -> Result<Url> {
    let timer = Timer::start();
    let base_url = env::var("BASE_URL")
        .context("Please set a BASE_URL environment variable")?;
    let mut base_url = Url::parse(base_url.trim_end_matches('/'))?;
    let host = base_url.host_str().ok_or(anyhow!("cannot parse host part of BASE_URL {}", base_url))?;
    let _ = tokio::net::lookup_host(format!("{}:443", host)).await?;

    base_url.set_path("/api/alexa/smart_home");

    timer.end("lookup_url() resolved IP");
    Ok(base_url)
}

async fn build_reqwest_client(event: &LambdaEvent<Value>) -> Result<(reqwest::Client, String)> {
    let timer = Timer::start();
    if tracing::enabled!(Level::TRACE) {
        let evt = serde_json::to_string_pretty(&event.payload)?;
        tracing::trace!("Event: {}", evt);
    }

    let directive = &event.payload["directive"];
    if directive.is_null() {
        bail!("Malformed request - missing directive");
    }
    let payload_version = directive["header"]["payloadVersion"].as_str().unwrap_or_default();
    if payload_version != "3" {
        bail!("Only payloadVersion == \"3\" is supported, got {}", payload_version);
    }

    let mut scope = &directive["endpoint"]["scope"];
    if scope.is_null() {
        scope = &directive["payload"]["grantee"];
    }
    if scope.is_null() {
        scope = &directive["payload"]["scope"];
    }
    if scope.is_null() {
        bail!("Malformed request - missing one between endpoint.scope, payload.grantee, or payload.scope");
    }
    if scope["type"].as_str().unwrap_or_default() != "BearerToken" {
        bail!("Malformed request - endpoint.scope.type only supports BearerToken");
    }

    let token = &scope["token"].as_str();
    let token = if token.is_none() && tracing::enabled!(Level::DEBUG) {
        env::var("LONG_LIVED_ACCESS_TOKEN").context("No token found in event, please provide a LONG_LIVEDF_ACCESS_TOKEN instead")?
    } else {
        token.ok_or(anyhow!("Malformed request - missing auth token"))?.into()
    };

    let disable_ssl_verification = if let Ok(v) = env::var("NOT_VERIFY_SSL") {
        v.parse().unwrap_or(false)
    } else {
        false
    };

    let client = reqwest::ClientBuilder::new()
        .connect_timeout(Duration::from_secs(2))
        .read_timeout(Duration::from_secs(10))
        .danger_accept_invalid_certs(disable_ssl_verification)
        .build()?;
    timer.end("build_request_client() completed");
    Ok((client, token))
}

async fn handler(event: LambdaEvent<Value>) -> Result<Value> {
    tracing::info!("Handler invoked");

    let (base_url, (client, token)) = tokio::try_join!(
        lookup_url(),
        build_reqwest_client(&event))?;

    tracing::info!("Resolved base URL: {}", base_url);

    let mut request = client.post(base_url.as_str())
        .header("Authorization", format!("Bearer {}", token));

    for (name, value) in parse_custom_headers()? {
        tracing::debug!("Adding custom header: {} = {}", name, value);
        request = request.header(&name, &value);
    }

    tracing::debug!("Sending request to Home Assistant");
    let response = request
        .json(&event.payload)
        .send()
        .await?;
    let response_status = response.status();
    tracing::info!("Home Assistant responded with status: {}", response_status);

    if !response_status.is_success() {
        let body = response.text().await?;
        tracing::warn!("Home Assistant error response ({}): {}", response_status, body);
        let val = ResponseData {
            event: EventData {
                payload: PayloadData {
                    t: (if [401, 403].contains(&response_status.as_u16()) {
                            "INVALID_AUTHORIZATION_CREDENTIAL"
                        } else {
                            "INTERNAL_ERROR"
                        }).to_owned(),
                    message: body,
                }
            }
        };
        return Ok(serde_json::to_value(&val)?);
    }

    let result = response.json::<Value>().await?;
    tracing::debug!("Handler completed successfully");
    Ok(result)
}

async fn logging_handler(event: LambdaEvent<Value>) -> Result<Value> {
    match handler(event).await {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::error!("Handler error: {:?}", e);
            Err(e)
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    tracing::info!("Lambda function starting up");

    lambda_runtime::run(service_fn(logging_handler)).await
}
