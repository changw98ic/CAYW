use crate::{PROTOCOL_VERSION, RpcError};
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeSeed},
};
use serde_json::{Value, json};
use std::{collections::HashSet, fmt, io, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    time::{Instant, timeout_at},
};

pub(crate) const HELLO_ID: &str = "h";
pub(crate) const CANCEL_METHOD: &str = "rpc.cancel";
pub(crate) const MAX_METHOD_BYTES: usize = 256;
// Counts all object/array containers, including the outer protocol object.
pub(crate) const MAX_JSON_DEPTH: usize = 64;

/// Validate the complete JSON before any Value can discard a duplicate key.
/// The first pass retains only keys in currently open objects, not a second tree.
/// Typed decoding still rejects unknown fields, missing fields and wrong types.
pub(crate) fn decode<T: de::DeserializeOwned>(bytes: &[u8], max: usize) -> io::Result<T> {
    if bytes.len() > max {
        return Err(invalid_json());
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    StrictJson { depth: 0 }
        .deserialize(&mut deserializer)
        .map_err(|_| invalid_json())?;
    deserializer.end().map_err(|_| invalid_json())?;
    serde_json::from_slice(bytes).map_err(|_| invalid_json())
}

fn invalid_json() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid bounded IPC JSON")
}

#[derive(Clone, Copy)]
struct StrictJson {
    depth: usize,
}

impl StrictJson {
    fn child<E: de::Error>(self) -> Result<Self, E> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(E::custom("IPC JSON nesting limit exceeded"));
        }
        Ok(Self {
            depth: self.depth + 1,
        })
    }
}

impl<'de> DeserializeSeed<'de> for StrictJson {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> de::Visitor<'de> for StrictJson {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded JSON without duplicate object keys")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_string<E: de::Error>(self, _: String) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<M: de::SeqAccess<'de>>(self, mut sequence: M) -> Result<(), M::Error> {
        let child = self.child()?;
        while sequence.next_element_seed(child)?.is_some() {}
        Ok(())
    }

    fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        let child = self.child()?;
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            // Keys are compared after JSON escape decoding; x and \\u0078 collide.
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate IPC JSON object key"));
            }
            map.next_value_seed(child)?;
        }
        Ok(())
    }
}

/// Values created by callers have no repeated keys, but can exceed decoder depth.
/// Bound local params/results before recursively serializing them onto the wire.
pub(crate) fn validate_value(value: &Value, depth: usize) -> io::Result<()> {
    match value {
        Value::Array(array) => {
            if depth >= MAX_JSON_DEPTH {
                return Err(invalid_json());
            }
            for value in array {
                validate_value(value, depth + 1)?;
            }
        }
        Value::Object(object) => {
            if depth >= MAX_JSON_DEPTH {
                return Err(invalid_json());
            }
            for value in object.values() {
                validate_value(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub version: u32,
    pub id: String,
    pub method: String,
    pub params: Value,
}

impl Request {
    pub fn hello(token: String) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id: HELLO_ID.into(),
            method: "hello".into(),
            params: json!({"token":token}),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HelloParams {
    pub token: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HelloResult {
    pub version: u32,
    pub max_frame_bytes: usize,
    pub max_in_flight: usize,
    pub max_requests_per_connection: usize,
    pub max_id_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancelParams {
    pub request_id: String,
}

#[derive(Serialize)]
pub(crate) struct Response {
    pub version: u32,
    pub id: String,
    #[serde(flatten)]
    pub payload: Payload,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Payload {
    Result { result: Value },
    Error { error: RpcError },
}

impl Response {
    pub fn new(id: String, result: Result<Value, RpcError>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            payload: match result {
                Ok(result) => Payload::Result { result },
                Err(error) => Payload::Error { error },
            },
        }
    }

    pub fn into_result(self) -> Result<Value, RpcError> {
        match self.payload {
            Payload::Result { result } => Ok(result),
            Payload::Error { error } => Err(error),
        }
    }
}

// Reject duplicate/unknown fields and result+error, while preserving an explicit null result.
impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Response;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an RPC response")
            }
            fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Response, M::Error> {
                let mut version = None;
                let mut id = None;
                let mut payload = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "version" if version.is_none() => version = Some(map.next_value()?),
                        "id" if id.is_none() => id = Some(map.next_value()?),
                        "result" if payload.is_none() => {
                            payload = Some(Payload::Result {
                                result: map.next_value()?,
                            })
                        }
                        "error" if payload.is_none() => {
                            payload = Some(Payload::Error {
                                error: map.next_value()?,
                            })
                        }
                        _ => {
                            return Err(de::Error::custom(
                                "unexpected or duplicate response field",
                            ));
                        }
                    }
                }
                Ok(Response {
                    version: version.ok_or_else(|| de::Error::missing_field("version"))?,
                    id: id.ok_or_else(|| de::Error::missing_field("id"))?,
                    payload: payload
                        .ok_or_else(|| de::Error::custom("missing response result/error"))?,
                })
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

pub(crate) fn valid_id(id: &str, max: usize) -> bool {
    !id.is_empty() && id.len() <= max && !id.chars().any(char::is_control)
}

pub(crate) fn valid_method(method: &str) -> bool {
    !method.is_empty() && method.len() <= MAX_METHOD_BYTES && !method.chars().any(char::is_control)
}

/// Fixed-size I/O buffer plus a partial frame that survives cancelled `next` futures.
/// The partial allocation grows only after checking the frame limit.
pub(crate) struct Frames<R> {
    reader: BufReader<R>,
    partial: Vec<u8>,
    max: usize,
    frame_timeout: Option<Duration>,
    deadline: Option<Instant>,
}

impl<R: AsyncRead + Unpin> Frames<R> {
    pub fn new(reader: R, max: usize, frame_timeout: Option<Duration>) -> Self {
        Self {
            reader: BufReader::with_capacity(4096, reader),
            partial: Vec::new(),
            max,
            frame_timeout,
            deadline: None,
        }
    }

    pub fn set_limit(&mut self, max: usize) {
        self.max = max;
    }

    pub fn has_buffered_bytes(&self) -> bool {
        !self.partial.is_empty() || !self.reader.buffer().is_empty()
    }

    pub async fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.deadline.is_none() {
            self.deadline = self.frame_timeout.map(|duration| Instant::now() + duration);
        }
        loop {
            let bytes = match self.deadline {
                Some(deadline) => timeout_at(deadline, self.reader.fill_buf())
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "IPC frame deadline exceeded")
                    })??,
                None => self.reader.fill_buf().await?,
            };
            if bytes.is_empty() {
                if self.partial.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated IPC frame",
                ));
            }
            let newline = bytes.iter().position(|&byte| byte == b'\n');
            let count = newline.unwrap_or(bytes.len());
            if count > self.max.saturating_sub(self.partial.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "IPC frame too large",
                ));
            }
            self.partial
                .try_reserve_exact(count)
                .map_err(|_| io::Error::other("cannot allocate IPC frame"))?;
            self.partial.extend_from_slice(&bytes[..count]);
            self.reader.consume(count + usize::from(newline.is_some()));
            if newline.is_some() {
                self.deadline = None;
                if self.partial.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "empty IPC frame",
                    ));
                }
                return Ok(Some(std::mem::take(&mut self.partial)));
            }
        }
    }
}

/// Serializes directly into a bounded allocation, rather than allocating first then checking.
pub(crate) fn encode(value: &impl Serialize, max: usize) -> io::Result<Vec<u8>> {
    struct Limited {
        bytes: Vec<u8>,
        max: usize,
    }
    impl io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.max.saturating_sub(self.bytes.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "IPC frame too large",
                ));
            }
            // Keep the capacity bounded even when the serializer emits many tiny writes.
            if self.bytes.len() + bytes.len() > self.bytes.capacity() {
                let target = (self.bytes.capacity().saturating_mul(2))
                    .max(self.bytes.len() + bytes.len())
                    .min(self.max);
                self.bytes
                    .try_reserve_exact(target - self.bytes.len())
                    .map_err(|_| io::Error::other("cannot allocate IPC frame"))?;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited {
        bytes: Vec::new(),
        max,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot encode bounded IPC frame",
        )
    })?;
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}
