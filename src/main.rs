#![feature(iterator_try_collect)]

mod templates;

use std::{env, fs::read_to_string, sync::Mutex};

use anyhow::{anyhow, bail};
use askama::Template;
use axum::{response::{Html, IntoResponse, Response}, routing::{get, post}, Json, Router};
use reqwest::StatusCode;
use serde_json::{json, Value};


static UP_ADDR: Mutex<String> = Mutex::new(String::new());
const PORT: u16 = 24542;


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd_args = env::args().collect::<Vec<String>>();
    if cmd_args.len() < 2 {
        bail!("Provide a provider address!");
    }
    let up_url = &cmd_args[1];
    *UP_ADDR.lock().unwrap() = up_url.clone();
    let app = Router::new()
        .route("/", get(index))
        .route("/main.css", get(css))
        .route("/api", post(in_prompt));

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", PORT)).await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}


async fn index() -> Result<Html<String>, InternalError> {
    //Ok(Html(templates::IndexTemplate{}.render()?))
    Ok(Html(read_to_string("templates/index.html")?))
}

async fn css() -> Result<Response<String>, InternalError> {
    Ok(Response::builder()
        .header("Content-Type", "text/css")
        .body(read_to_string("static/main.css")?)?)
}

async fn in_prompt(Json(payload): Json<serde_json::Value>) -> Result<Response, InternalError> {
    println!("Recv req {:?}", payload);
    match extract_messages(payload) {
        Ok(messages) => Ok(handle_prompt(messages).await?),
        Err(s) => Err(InternalError(anyhow!(s), StatusCode::BAD_REQUEST))
    }
}

fn extract_messages(payload: Value) -> Result<Vec<(String, String)>, String> {
    match payload {
        Value::Array(a) => {
            a.iter().map(|v| match v {
                Value::Object(o) => {
                    match (o.get("content"), o.get("role")) {
                        (Some(Value::String(b)), Some(Value::String(r))) => {
                            if r == "user" || r == "assistant" || r == "system" {
                                Ok((b.clone(), r.clone()))
                            } else {
                                Err(format!("Invalid role {} given", r))
                            }
                        }
                        _ => Err(format!("Message {:?} does not contain content or role fields", o))
                    }
                }
                _ => Err(format!("Malformed array: {:?} is not an object", v))
            }).try_collect()
        }
        _ => Err("Payload is not an array".to_string())
    }
}

async fn handle_prompt(messages: Vec<(String, String)>) -> Result<Response, InternalError> {
    fn to_message(message: &(String, String)) -> Value {
        match message {
            (content, role) => json!({
                "role": role,
                "content": content,
            })
        }
    }
    let req_json = json!({
        "model": "qwen3:8b",
        "messages": messages.iter().map(to_message).collect::<Vec<Value>>(),
        "think": false,
        "stream": false
    });
    let addr = UP_ADDR.lock().unwrap().clone();
    let client = reqwest::Client::new();
    println!("http://{}/api/chat", addr);
    let resp_str = client.post(format!("http://{}/api/chat", addr))
        .json(&req_json)
        .send()
        .await.map_err(|e| anyhow!("Error interfacing with upstream server, {:?}", e))?
        .text().await.map_err(|e| anyhow!("Error acquiring response from upstream server, {:?}", e))?;
        
    let resp_json: Value = serde_json::from_str(resp_str.as_str()).map_err(|e| anyhow!("Could not interpret upstream response, {:?}", e))?;
    match resp_json.get("message") {
        Some(o) => 
            match (o.get("role"), o.get("content")) {
                (Some(Value::String(role)), Some(Value::String(content))) => Ok(
                    Json(json!({
                        "role": role,
                        "content": content,
                    })).into_response()
                ),
                _ => Err(InternalError(anyhow!("Received error from upstream: {:?}", resp_json), StatusCode::INTERNAL_SERVER_ERROR))
            }
        _ => Err(InternalError(anyhow!("Received error from upstream: {:?}", resp_json), StatusCode::INTERNAL_SERVER_ERROR))
    }
}


struct InternalError(anyhow::Error, StatusCode);

impl IntoResponse for InternalError {
    fn into_response(self) -> Response {
        match self {
            InternalError(e, code) => (code, Json(json!({
                "error": format!("Something went wrong: {}", e)
                }))).into_response()
        }
    }
}

impl<E> From<E> for InternalError
    where E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into(), StatusCode::INTERNAL_SERVER_ERROR)
    }
}
