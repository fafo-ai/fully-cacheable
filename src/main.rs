use actix_web::{web, get, App, HttpServer, HttpResponse, Error, HttpRequest, Responder};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use base64::prelude::*;
/*
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
 */


const PORT: u16 = 4567;

struct AppState {
    embedding_cache: Mutex<HashMap<String, Vec<u8>>>,
}

#[get("/status")]
async fn handle_status() -> impl Responder {
    HttpResponse::Ok().body("up")
}

async fn proxy_request(
    req: HttpRequest,
    mut req_body: web::Json<Value>,
    state: web::Data<AppState>,
    client: web::Data<reqwest::Client>,
) -> Result<HttpResponse, Error> {

    let to_base = "api.openai.com";
    let from = req.full_url();
    let mut to = from.clone();
    to.set_host(Option::from(to_base)).unwrap();
    to.set_scheme("https").unwrap();
    to.set_port(Some(443)).unwrap();

    println!("Got request: {:?} -> {:?}", from.as_str(), to.as_str());

    let openai_api_key = req.headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Missing API key"))?
        .to_string();


    if from.path().to_string() == "/v1/embeddings" {
        let input = req_body["input"].as_str().unwrap_or("");
        let cache_key = input.to_string();
        let mut cache = state.embedding_cache.lock().unwrap();

        let old_format = req_body["encoding_format"].as_str().map(|x| x.to_string()).unwrap_or("text".into());

        print!("Call to embeddings API.");
        let result = if let Some(cached_embedding) = cache.get(&cache_key) {
            println!("Cache hit!");
            cached_embedding.clone()
        } else {
            println!("Cache miss..");

            req_body.as_object_mut().unwrap().insert("encoding_format".into(), json!("base64"));
            let resp = client.post(to)
                .header(AUTHORIZATION, &openai_api_key)
                .timeout(Duration::from_secs(2))
                .json(&req_body)
                .send()
                .await
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

            /* Assert there's only one */
            let resp_json: Value = resp.json().await
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

            // println!("Coe: {:?}", resp_json);

            let embedding = BASE64_STANDARD.decode(
                resp_json["data"][0]["embedding"].as_str().unwrap()
            )
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

            println!("B");
            let x = embedding.clone();
            cache.insert(cache_key, x);
            println!("C");
            embedding
        };

        if old_format == "base64" {
            return Ok(HttpResponse::Ok().json(json!({
                "data": [{
                    "embedding": BASE64_STANDARD.encode(&result),
                    "index": 0,
                    "object": "embedding"
                }],
                "model": req_body["model"],
                "object": "list",
                "usage": {
                    "prompt_tokens": result.len() / 4,
                    "total_tokens": result.len() / 4,
                }
            })));
        } else if old_format == "float" {
            let ret: Vec<f32> = result
                .chunks_exact(4)
                .map(|chunk| {
                    let arr: [u8; 4] = chunk.try_into().unwrap();
                    f32::from_le_bytes(arr)
                })
                .collect();

            return Ok(HttpResponse::Ok().json(json!({
                "data": [{
                    "embedding": ret,
                    "index": 0,
                    "object": "embedding"
                }],
                "model": req_body["model"],
                "object": "list",
                "usage": {
                    "prompt_tokens": result.len() / 4,
                    "total_tokens": result.len() / 4,
                }
            })))
        } else {
            todo!()
        }
    } else {
        println!("Just proxying...");
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&openai_api_key)
            .map_err(|e| actix_web::error::ErrorInternalServerError(e))?);
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        let stream = req_body["stream"].as_bool().unwrap_or(false);

        if stream {
            todo!();
            /*
            let es = EventSource::new(
                reqwest::Client::builder()
                    .default_headers(headers)
                    .build()
                    .unwrap()
                    .post("https://api.openai.com/v1/chat/completions")
                    .json(&req_body)
            ).unwrap();

            let (tx, rx) = actix_web::web::Bytes::new_channel();

            actix_web::rt::spawn(async move {
                let mut es = es;
                while let Some(event) = es.next().await {
                    match event {
                        Ok(Event::Message(message)) => {
                            if message.data == "[DONE]" {
                                break;
                            }
                            let _ = tx.send(actix_web::web::Bytes::from(message.data + "\n"));
                        }
                        Ok(Event::Open) => continue,
                        Err(err) => {
                            eprintln!("Error: {:?}", err);
                            break;
                        }
                    }
                }
            });

            Ok(HttpResponse::Ok()
                .content_type("text/event-stream")
                .streaming(rx))
             */
        } else {
            let resp = client.post(to)
                .headers(headers)
                .json(&req_body)
                .send()
                .await
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

            let resp_json: Value = resp.json().await
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
            Ok(HttpResponse::Ok().json(resp_json))
        }
    }
}

#[tokio::main] // By default, tokio_postgres uses the tokio crate as its runtime.
async fn main() -> std::io::Result<()>{
    let app_state = web::Data::new(AppState {
        embedding_cache: Mutex::new(HashMap::new()),
    });

    let client = web::Data::new(reqwest::Client::new());

    println!("Server listening on port: {}", PORT);
    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .app_data(client.clone())
            .service(handle_status)
            .route("/v1/{endpoint:.*}", web::post().to(proxy_request))
    })
        .bind(("127.0.0.1", PORT))?
        .run()
        .await
}
