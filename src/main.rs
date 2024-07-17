use actix_web::{web, App, HttpServer, HttpResponse, Error, HttpRequest};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest_eventsource::{Event, EventSource};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

struct AppState {
    embedding_cache: Mutex<HashMap<String, Vec<f32>>>,
}

async fn proxy_request(
    req: HttpRequest,
    req_body: web::Json<Value>,
    state: web::Data<AppState>,
    client: web::Data<reqwest::Client>,
) -> Result<HttpResponse, Error> {
    let openai_api_key = req.headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Missing API key"))?
        .to_string();

    let openai_url = "https://api.openai.com/v1/embeddings";

    if req_body["model"].as_str().unwrap_or("").contains("embed") {
        let input = req_body["input"].as_str().unwrap_or("");
        let mut cache = state.embedding_cache.lock().unwrap();

        if let Some(cached_embedding) = cache.get(input) {
            return Ok(HttpResponse::Ok().json(json!({
                "data": [{
                    "embedding": cached_embedding,
                    "index": 0,
                    "object": "embedding"
                }],
                "model": req_body["model"],
                "object": "list",
                "usage": {
                    "prompt_tokens": input.split_whitespace().count(),
                    "total_tokens": input.split_whitespace().count()
                }
            })));
        }

        let resp = client.post(openai_url)
            .header(AUTHORIZATION, &openai_api_key)
            .json(&req_body)
            .send()
            .await
            .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

        let mut resp_json: Value = resp.json().await
            .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
        let embedding = resp_json["data"][0]["embedding"].as_array().unwrap().to_vec();
        let embedding: Vec<f32> = embedding.into_iter().map(|v| v.as_f64().unwrap() as f32).collect();

        cache.insert(input.to_string(), embedding.clone());

        Ok(HttpResponse::Ok().json(resp_json))
    } else {
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
            let resp = client.post("https://api.openai.com/v1/chat/completions")
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

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let app_state = web::Data::new(AppState {
        embedding_cache: Mutex::new(HashMap::new()),
    });

    let client = web::Data::new(reqwest::Client::new());

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .app_data(client.clone())
            .route("/v1/{endpoint:.*}", web::post().to(proxy_request))
    })
        .bind("127.0.0.1:8080")?
        .run()
        .await
}