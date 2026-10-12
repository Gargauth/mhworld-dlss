//! Requests, responses and errors (`docs/DEV-PROTOCOL.md` § Request /
//! response), and validation of request parameters.

use std::fmt;

use serde_json::{json, Map, Value};

/// The error codes of the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Malformed request or parameters.
    BadRequest,
    /// Missing or wrong token.
    Unauthorized,
    /// No such method.
    UnknownMethod,
    /// The subsystem has not started (or is not compiled in).
    NotReady,
    /// The execution point did not come in time.
    Timeout,
    /// Not allowed (e.g. a write outside the engine structs).
    Refused,
    /// The server failed.
    Internal,
}

impl ErrorCode {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::UnknownMethod => "unknown_method",
            ErrorCode::NotReady => "not_ready",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Refused => "refused",
            ErrorCode::Internal => "internal",
        }
    }
}

/// An `ok: false` answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    /// Its code.
    pub code: ErrorCode,
    /// A human-readable explanation.
    pub message: String,
}

impl RpcError {
    /// An error with `code`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> RpcError {
        RpcError {
            code,
            message: message.into(),
        }
    }

    /// `bad_request`.
    pub fn bad_request(message: impl Into<String>) -> RpcError {
        RpcError::new(ErrorCode::BadRequest, message)
    }

    /// `not_ready`.
    pub fn not_ready(message: impl Into<String>) -> RpcError {
        RpcError::new(ErrorCode::NotReady, message)
    }

    /// `refused`.
    pub fn refused(message: impl Into<String>) -> RpcError {
        RpcError::new(ErrorCode::Refused, message)
    }

    /// `timeout`.
    pub fn timeout(message: impl Into<String>) -> RpcError {
        RpcError::new(ErrorCode::Timeout, message)
    }

    /// `internal`.
    pub fn internal(message: impl Into<String>) -> RpcError {
        RpcError::new(ErrorCode::Internal, message)
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

/// Result of a method.
pub type RpcResult<T> = Result<T, RpcError>;

/// Where a request runs (`docs/DEV-PROTOCOL.md` § Execution points).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Point {
    /// Start of a main-loop iteration.
    Frame,
    /// Inside the camera-setup detour, after the original.
    Camera,
    /// Inside the upscale slot, engine command list open.
    Slot,
    /// Before Present.
    Present,
}

impl Point {
    /// Every point, in [`Point::index`] order.
    pub const ALL: [Point; 4] = [Point::Frame, Point::Camera, Point::Slot, Point::Present];

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Point::Frame => "frame",
            Point::Camera => "camera",
            Point::Slot => "slot",
            Point::Present => "present",
        }
    }

    /// Position in [`Point::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The point named `name`.
    pub fn parse(name: &str) -> Option<Point> {
        Point::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

macro_rules! methods {
    ($($variant:ident = $name:literal),+ $(,)?) => {
        /// The protocol's methods.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Method { $(#[doc = $name] $variant),+ }

        impl Method {
            /// Every method.
            pub const ALL: &'static [Method] = &[$(Method::$variant),+];

            /// The wire name.
            pub const fn name(self) -> &'static str {
                match self { $(Method::$variant => $name),+ }
            }
        }
    };
}

methods! {
    Auth = "auth",
    Hello = "hello",
    State = "state",
    SettingsGet = "settings.get",
    SettingsSet = "settings.set",
    ExperimentSet = "experiment.set",
    Symbols = "symbols",
    MemRead = "mem.read",
    MemWrite = "mem.write",
    Watch = "watch",
    Unwatch = "unwatch",
    LogSubscribe = "log.subscribe",
    LogTail = "log.tail",
    Capture = "capture",
    ProbeJitter = "probe.jitter",
    ProbeReset = "probe.reset",
    ProbeHdr = "probe.hdr",
    ProbeMv = "probe.mv",
    ProbeDepth = "probe.depth",
    ProbeWait = "probe.wait",
    ProbeModules = "probe.modules",
    Trace = "trace",
}

impl Method {
    /// The method called `name`.
    pub fn parse(name: &str) -> Option<Method> {
        Method::ALL.iter().copied().find(|m| m.name() == name)
    }
}

/// A validated request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The client's id (an integer), echoed in every response part.
    pub id: Value,
    /// What to do.
    pub method: Method,
    /// `params`: an object (empty when absent), or a list for
    /// `settings.set`.
    pub params: Value,
    /// `params.point`, when given.
    pub point: Option<Point>,
}

/// Validate one request frame. On failure, the id to answer with (`null`
/// if the frame had none usable) and the error.
pub fn parse_request(msg: &Map<String, Value>) -> Result<Request, (Value, RpcError)> {
    let id = match msg.get("id") {
        Some(id @ Value::Number(n)) if n.is_i64() || n.is_u64() => id.clone(),
        _ => {
            return Err((
                Value::Null,
                RpcError::bad_request("request needs an integer id"),
            ))
        }
    };
    let fail = |error: RpcError| Err((id.clone(), error));
    let Some(name) = msg.get("method").and_then(Value::as_str) else {
        return fail(RpcError::bad_request("request needs a method name"));
    };
    let Some(method) = Method::parse(name) else {
        return fail(RpcError::new(
            ErrorCode::UnknownMethod,
            format!("no method {name:?}"),
        ));
    };
    let params = match msg.get("params") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(p @ Value::Array(_)) if method == Method::SettingsSet => p.clone(),
        Some(_) => return fail(RpcError::bad_request("params must be an object")),
    };
    let point = match params.get("point") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => match Point::parse(name) {
            Some(point) => Some(point),
            None => return fail(RpcError::bad_request(format!("unknown point {name:?}"))),
        },
        Some(_) => return fail(RpcError::bad_request("point must be a string")),
    };
    Ok(Request {
        id,
        method,
        params,
        point,
    })
}

/// A success response part; `more` marks all but the last part.
pub fn ok(id: &Value, result: Value, more: bool) -> Value {
    let mut out = json!({"id": id, "ok": true, "result": result});
    if more {
        out["more"] = Value::Bool(true);
    }
    out
}

/// An error response.
pub fn error(id: &Value, error: &RpcError) -> Value {
    json!({
        "id": id,
        "ok": false,
        "error": {"code": error.code.as_str(), "message": error.message},
    })
}

/// Typed access to a params object with protocol errors.
#[derive(Clone, Copy, Debug)]
pub struct Params<'a>(pub &'a Value);

impl<'a> Params<'a> {
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    /// Whether `key` is present (and not null).
    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Integer `key` in `low..=high`, `default` when absent.
    pub fn int(&self, key: &str, default: i64, low: i64, high: i64) -> RpcResult<i64> {
        let Some(value) = self.get(key) else {
            return Ok(default);
        };
        value
            .as_i64()
            .filter(|n| (low..=high).contains(n))
            .ok_or_else(|| RpcError::bad_request(format!("{key} must be an integer {low}..{high}")))
    }

    /// Count `key` in `low..=high` (both ≥ 0), `default` when absent.
    pub fn count(&self, key: &str, default: u32, low: u32, high: u32) -> RpcResult<u32> {
        let n = self.int(key, default.into(), low.into(), high.into())?;
        // In range of `u32` by construction.
        Ok(u32::try_from(n).unwrap_or(high))
    }

    /// Number `key` in `(low, high]`, `default` when absent.
    pub fn seconds(&self, key: &str, default: f64, low: f64, high: f64) -> RpcResult<f64> {
        let Some(value) = self.get(key) else {
            return Ok(default);
        };
        value
            .as_f64()
            .filter(|s| !value.is_boolean() && *s > low && *s <= high)
            .ok_or_else(|| {
                RpcError::bad_request(format!("{key} must be a number in ({low}, {high}]"))
            })
    }

    /// Required string `key`.
    pub fn str(&self, key: &str) -> RpcResult<&'a str> {
        self.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::bad_request(format!("{key} (a string) is required")))
    }

    /// Optional string `key`.
    pub fn opt_str(&self, key: &str) -> RpcResult<Option<&'a str>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(RpcError::bad_request(format!("{key} must be a string"))),
        }
    }

    /// Boolean `key`, `default` when absent.
    pub fn bool(&self, key: &str, default: bool) -> RpcResult<bool> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(RpcError::bad_request(format!(
                "{key} must be true or false"
            ))),
        }
    }

    /// Required value `key`.
    pub fn value(&self, key: &str) -> RpcResult<&'a Value> {
        self.get(key)
            .ok_or_else(|| RpcError::bad_request(format!("{key} is required")))
    }

    /// A list of strings (`[..]`, or one comma-separated string), empty
    /// when absent.
    pub fn strings(&self, key: &str) -> RpcResult<Vec<String>> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::String(s)) => Ok(s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| RpcError::bad_request(format!("{key} must list strings")))
                })
                .collect(),
            Some(_) => Err(RpcError::bad_request(format!(
                "{key} must be a list of strings"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: Value) -> Result<Request, (Value, RpcError)> {
        parse_request(value.as_object().unwrap())
    }

    #[test]
    fn a_full_request_parses() {
        let r = parse(json!({"id": 7, "method": "mem.read", "params": {"point": "slot"}})).unwrap();
        assert_eq!(r.id, json!(7));
        assert_eq!(r.method, Method::MemRead);
        assert_eq!(r.point, Some(Point::Slot));
        let r = parse(json!({"id": 1, "method": "state"})).unwrap();
        assert_eq!(r.params, json!({}));
        assert_eq!(r.point, None);
    }

    #[test]
    fn ids_must_be_integers_and_errors_echo_them() {
        for bad in [
            json!({"method": "state"}),
            json!({"id": "1", "method": "state"}),
            json!({"id": 1.5, "method": "state"}),
        ] {
            let (id, error) = parse(bad).unwrap_err();
            assert_eq!((id, error.code), (Value::Null, ErrorCode::BadRequest));
        }
        let (id, error) = parse(json!({"id": 3, "method": "nope"})).unwrap_err();
        assert_eq!((id, error.code), (json!(3), ErrorCode::UnknownMethod));
        let (_, error) = parse(json!({"id": 3})).unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
    }

    #[test]
    fn params_are_objects_except_for_a_settings_list() {
        let (_, error) = parse(json!({"id": 1, "method": "state", "params": [1]})).unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
        let r = parse(json!({"id": 1, "method": "settings.set", "params": [{"section": "a"}]}))
            .unwrap();
        assert!(r.params.is_array());
        let (_, error) =
            parse(json!({"id": 1, "method": "watch", "params": {"point": "draw"}})).unwrap_err();
        assert!(error.message.contains("draw"));
    }

    #[test]
    fn every_method_round_trips_its_name() {
        for &m in Method::ALL {
            assert_eq!(Method::parse(m.name()), Some(m));
        }
        for p in Point::ALL {
            assert_eq!(Point::parse(p.as_str()), Some(p));
            assert_eq!(Point::ALL[p.index()], p);
        }
    }

    #[test]
    fn param_helpers_check_types_and_ranges() {
        let v = json!({"frames": 4, "neg": -1, "flag": true, "s": 0.5, "big": 1e9, "t": "a,b", "l": ["x"]});
        let p = Params(&v);
        assert_eq!(p.count("frames", 1, 1, 64).unwrap(), 4);
        assert_eq!(p.count("absent", 9, 1, 64).unwrap(), 9);
        assert!(p.count("neg", 1, 0, 9).is_err());
        assert!(p.count("flag", 1, 0, 9).is_err());
        assert!(p.count("s", 1, 0, 9).is_err());
        assert_eq!(p.seconds("s", 1.0, 0.0, 60.0).unwrap(), 0.5);
        assert!(p.seconds("big", 1.0, 0.0, 60.0).is_err());
        assert!(p.seconds("flag", 1.0, 0.0, 60.0).is_err());
        assert_eq!(p.strings("t").unwrap(), ["a", "b"]);
        assert_eq!(p.strings("l").unwrap(), ["x"]);
        assert!(p.strings("frames").is_err());
        assert!(p.bool("flag", false).unwrap());
        assert!(p.str("frames").is_err());
    }

    #[test]
    fn responses_have_the_documented_shape() {
        assert_eq!(
            ok(&json!(2), json!({}), true),
            json!({"id": 2, "ok": true, "result": {}, "more": true})
        );
        assert_eq!(
            error(&json!(2), &RpcError::refused("no")),
            json!({"id": 2, "ok": false, "error": {"code": "refused", "message": "no"}})
        );
    }
}
