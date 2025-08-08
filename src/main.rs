#![feature(iterator_try_collect)]
use std::{env, fs::read_to_string, sync::Mutex};

use anyhow::{anyhow, bail};
use axum::{response::{IntoResponse, Response}, routing::{get, post}, Json, Router};
use reqwest::StatusCode;
use serde_json::{json, Value};


static UP_ADDR: Mutex<String> = Mutex::new(String::new());


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

    let listener = tokio::net::TcpListener::bind("0.0.0.0:80").await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}


async fn index() -> String {
    String::from("index")
}

async fn in_prompt(Json(payload): Json<serde_json::Value>) -> Result<Response, InternalError> {
    println!("Recv req {:?}", payload);
    match extract_messages(payload) {
        Ok(messages) => Ok(handle_prompt(messages).await?),
        Err(s) => Ok((StatusCode::BAD_REQUEST, s).into_response())
    }
}

fn extract_messages(payload: Value) -> Result<Vec<(String, String)>, String> {
    match payload {
        Value::Array(a) => {
            a.iter().map(|v| match v {
                Value::Object(o) => {
                    match (o["body"].clone(), o["role"].clone()) {
                        (Value::String(b), Value::String(r)) => {
                            if r == "user" || r == "assistant" || r == "system" {
                                Ok((b, r))
                            } else {
                                Err(format!("Invalid role {} given", r))
                            }
                        }
                        _ => Err(format!("Message {:?} does not contain body or role fields", o))
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
            (body, role) => json!({
                "role": role,
                "content": body,
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
    let resp_str = client.post(format!("{addr}/api/chat"))
        .json(&req_json)
        .send()
        .await.map_err(|e| anyhow!("Error interfacing with upstream server, {:?}", e))?
        .text().await.map_err(|e| anyhow!("Error acquiring response from upstream server, {:?}", e))?;
        
    let resp_json: Value = serde_json::from_str(resp_str.as_str()).map_err(|e| anyhow!("Could not interpret upstream response, {:?}", e))?;
    match (&resp_json["message"]["role"], &resp_json["message"]["content"]) {
        (Value::String(role), Value::String(content)) => Ok(
            Json(json!({
                "role": role,
                "content": content,
            })).into_response()
        ),
        _ => Err(InternalError(anyhow!("Received error from upstream: {:?}", resp_json)))
    }
}


async fn css() -> Result<String, StatusCode> {
    read_to_string("static/main.css").map_err(|_| {StatusCode::INTERNAL_SERVER_ERROR})
}

struct InternalError(anyhow::Error);

impl IntoResponse for InternalError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Something went wrong: {}", self.0)).into_response()
    }
}

impl<E> From<E> for InternalError
    where E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
