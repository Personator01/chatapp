#![feature(iterator_try_collect)]

mod templates;

use std::{collections::HashMap, env, fs::read_to_string, sync::{Arc, RwLock}, time::Instant};

use anyhow::{anyhow, bail};
use axum::{extract::{Path, State}, response::{Html, IntoResponse, Response}, routing::{get, post}, Json, Router};
use rand::{Rng, SeedableRng};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::{io::AsyncBufReadExt, sync::{mpsc::{channel, Receiver, Sender}, Mutex}, task::JoinHandle};
use tokio_util::io::StreamReader;
use futures::stream::TryStreamExt;


static UP_ADDR: RwLock<String> = RwLock::new(String::new());
const PORT: u16 = 24542;


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd_args = env::args().collect::<Vec<String>>();
    if cmd_args.len() < 2 {
        bail!("Provide a provider address!");
    }

    let app_state = Arc::new(AppState{ message_streams: Mutex::new(HashMap::new()), rng: Mutex::new(rand::rngs::SmallRng::from_os_rng()) });

    let up_url = &cmd_args[1];
    *UP_ADDR.write().unwrap() = up_url.clone();
    let app = Router::new()
        .route("/", get(index))
        .route("/main.css", get(css))
        .route("/api", post(in_nostream))
        .route("/api/streaming", post(in_streaming))
        .route("/api/streaming/{endpoint}", get(in_streampart))
        .with_state(app_state);

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

async fn in_nostream(Json(payload): Json<serde_json::Value>) -> Result<Response, InternalError> {
    println!("Recv req {:?}", payload);
    match extract_messages(payload) {
        Ok(messages) => Ok(handle_prompt(messages).await?),
        Err(s) => Err(InternalError(anyhow!(s), StatusCode::BAD_REQUEST))
    }
}

async fn in_streaming(State(state): State<Arc<AppState>>, Json(payload): Json<serde_json::Value>) -> Result<Response, InternalError> {
    println!("Recv streaming req {:?}", payload);
    match extract_messages(payload) {
        Ok(messages) => Ok(handle_stream(messages, state).await?),
        Err(s) => Err(InternalError(anyhow!(s), StatusCode::BAD_REQUEST))
    }
}

async fn in_streampart(State(state): State<Arc<AppState>>, Path(endpoint): Path<u32>) -> Result<Response, InternalError> {
    let mut streams_h = state.message_streams.lock().await;
    match streams_h.get_mut(&endpoint) {
        Some(s) => {
            match (s.messages.recv().await) {
                Some(message) => Ok(Json::<serde_json::Value>(message).into_response()),
                None => Ok(StatusCode::NO_CONTENT.into_response())
            }
        }
        None => Err(InternalError(anyhow!("Invalid endpoint {endpoint}"), StatusCode::BAD_REQUEST))
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

fn to_message(message: &(String, String)) -> Value {
    match message {
        (content, role) => json!({
            "role": role,
            "content": content,
        })
    }
}

async fn handle_prompt(messages: Vec<(String, String)>) -> Result<Response, InternalError> {
    let req_json = json!({
        "model": "qwen3:8b",
        "messages": messages.iter().map(to_message).collect::<Vec<Value>>(),
        "think": false,
        "stream": false
    });
    let addr = UP_ADDR.read().unwrap().clone();
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


struct StreamingValue {
    spawned: Instant,
    task: JoinHandle<()>,
    messages: Receiver<Value>
}

struct AppState {
    message_streams: Mutex<HashMap<u32, StreamingValue>>,
    rng: Mutex<rand::rngs::SmallRng>
}

async fn handle_stream(messages: Vec<(String, String)>, state: Arc<AppState>) -> Result<Response, InternalError> {

    let req_json = json!({
        "model": "qwen3:8b",
        "messages": messages.iter().map(to_message).collect::<Vec<Value>>(),
        "think": false,
        "stream": true
    });

    let mut rand_h = state.rng.lock().await;
    let mut streams_h = state.message_streams.lock().await;

    let mut id: u32 = rand_h.random();
    let mut counter: u32 = 0;
    while streams_h.contains_key(&id) && counter < 100 {
        id = rand_h.random();
        counter += 1;
    }
    if counter == 100 {
       return Err(anyhow!("Couldn't find a free place in the container somehow").into());
    }
    let (snd, recv) = channel(100);
    streams_h.insert(id, StreamingValue{
        spawned: Instant::now(),
        task: tokio::spawn(stream_cb(snd, req_json)),
        messages: recv
    });
    return Ok(Json(json!({"endpoint": id})).into_response());
}


async fn stream_cb(dest: Sender<Value>, body: Value) {
    async fn stream_cb_inner(dest: Sender<Value>, body: Value) -> anyhow::Result<()> {
        let client = reqwest::Client::new();
        let addr = UP_ADDR.read().unwrap().clone();
        println!("http://{}/api/chat", addr);
        let resp_stream =  
            client.post(format!("http://{}/api/chat", addr))
            .json(&body)
            .send()
            .await?
            .bytes_stream();
        let mut resp_reader = StreamReader::new(resp_stream.map_err(conv_err)).lines();
        while let Some(line) = resp_reader.next_line().await? {
            let resp_json: Value = serde_json::from_str(line.as_str()).map_err(|e| anyhow!("Could not interpret upstream response, {:?}", e))?;
            println!("upstream {}", resp_json);
            match (resp_json.get("done")) {
                Some(Value::Bool(true)) => {return Ok(());},
                Some(Value::Bool(false)) => match resp_json.get("message") {
                    Some(o) => 
                        match (o.get("role"), o.get("content")) {
                            (Some(Value::String(role)), Some(Value::String(content))) => {
                                dest.send(json!({"content": content})).await.unwrap();
                            },
                            _ => bail!("Received error from upstream: {:?}", resp_json)
                        }
                    _ => bail!("Received error from upstream: {:?}", resp_json)
                }
                _ => bail!("Could not interpret {} as message", line)
            }
            println!("done?");
        }
        println!("done {:?}", resp_reader.next_line().await?);
        return Ok(());
    }

    match stream_cb_inner(dest.clone(), body).await {
        Err(e) => {
            dest.send(json!({"error": format!("Error interacting with upstream: {}", e)})).await.unwrap();
        }
        Ok(()) => {}
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

fn conv_err(err: reqwest::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, format!("Error retrieving response from upstream, {}", err))
}
