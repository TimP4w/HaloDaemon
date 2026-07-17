// SPDX-License-Identifier: GPL-3.0-or-later
//! The `halod.http` capability API. A plugin that declares a scoped `http`
//! transport and holds the `network` permission gets `halod.http:request{…}`,
//! a synchronous bounded request checked against its declared origins before a
//! socket opens. This is a capability global (like `halod.publish`), not the
//! per-device `transport:` userdata — the plugin has no persistent socket.

use std::time::Duration;

use mlua::{Lua, Table, Value};

use crate::services::http::{HttpRequest, HttpResponse, HttpRuntime};

pub fn register(lua: &Lua, runtime: HttpRuntime) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    let http = lua.create_table()?;
    http.set(
        "request",
        // `halod.http:request(args)` passes the http table itself as `_self`.
        lua.create_function(move |lua, (_self, args): (Table, Table)| {
            let req = parse_request(&args)?;
            let response = runtime
                .request(req)
                .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
            response_to_lua(lua, response)
        })?,
    )?;
    halod.set("http", http)?;
    Ok(())
}

fn parse_request(args: &Table) -> mlua::Result<HttpRequest> {
    let method: String = args
        .get::<Option<String>>("method")?
        .unwrap_or_else(|| "GET".into());
    let origin: String = args
        .get::<Option<String>>("origin")?
        .ok_or_else(|| mlua::Error::RuntimeError("http request requires an 'origin'".into()))?;
    let path: String = args.get::<Option<String>>("path")?.unwrap_or_default();
    let mut headers = Vec::new();
    if let Some(table) = args.get::<Option<Table>>("headers")? {
        for pair in table.pairs::<String, String>() {
            let (name, value) = pair?;
            headers.push((name, value));
        }
    }
    let body = match args.get::<Value>("body")? {
        Value::Nil => Vec::new(),
        Value::String(s) => s.as_bytes().to_vec(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "http request 'body' must be a string".into(),
            ))
        }
    };
    let timeout = Duration::from_millis(args.get::<Option<u64>>("timeout_ms")?.unwrap_or(0));
    Ok(HttpRequest {
        method: method.to_ascii_uppercase(),
        origin,
        path,
        headers,
        body,
        timeout,
    })
}

fn response_to_lua(lua: &Lua, response: HttpResponse) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("status", response.status)?;
    let headers = lua.create_table()?;
    for (name, value) in response.headers {
        headers.set(name.to_ascii_lowercase(), value)?;
    }
    table.set("headers", headers)?;
    table.set("body", lua.create_string(&response.body)?)?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::http::{HttpBackend, HttpPolicy};
    use std::sync::{Arc, Mutex};

    struct StubBackend {
        last: Mutex<Option<HttpRequest>>,
        response: HttpResponse,
    }

    impl HttpBackend for StubBackend {
        fn request(&self, req: &HttpRequest, _max: usize) -> anyhow::Result<HttpResponse> {
            *self.last.lock().unwrap() = Some(req.clone());
            Ok(self.response.clone())
        }
    }

    fn runtime(backend: Arc<StubBackend>) -> HttpRuntime {
        let config = crate::plugin::manifest::HttpConfig {
            origins: vec!["https://api.example.com".into()],
            host_key: None,
            methods: vec!["GET".into(), "POST".into()],
            max_request_bytes: 1024,
            max_response_bytes: 1024,
            max_timeout_ms: 5000,
            max_concurrency: 2,
            allow_private: false,
            tls: None,
            pairing: None,
        };
        HttpRuntime::new(HttpPolicy::from_config(&config, None), backend, 2)
    }

    #[test]
    fn request_reaches_backend_and_returns_status_and_body() {
        let backend = Arc::new(StubBackend {
            last: Mutex::new(None),
            response: HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: b"{\"ok\":true}".to_vec(),
            },
        });
        let lua = Lua::new();
        lua.globals()
            .set("halod", lua.create_table().unwrap())
            .unwrap();
        register(&lua, runtime(backend.clone())).unwrap();
        let body: String = lua
            .load(
                r#"local r = halod.http:request{ method = "get", origin = "https://api.example.com", path = "/v1/status" }
                   assert(r.status == 200)
                   assert(r.headers["content-type"] == "application/json")
                   return r.body"#,
            )
            .eval()
            .unwrap();
        assert_eq!(body, "{\"ok\":true}");
        let seen = backend.last.lock().unwrap().clone().unwrap();
        assert_eq!(seen.method, "GET");
        assert_eq!(seen.path, "/v1/status");
    }

    #[test]
    fn undeclared_origin_is_rejected_before_the_backend() {
        let backend = Arc::new(StubBackend {
            last: Mutex::new(None),
            response: HttpResponse {
                status: 200,
                headers: vec![],
                body: vec![],
            },
        });
        let lua = Lua::new();
        lua.globals()
            .set("halod", lua.create_table().unwrap())
            .unwrap();
        register(&lua, runtime(backend.clone())).unwrap();
        let err = lua
            .load(r#"return halod.http:request{ origin = "https://evil.example.com" }"#)
            .eval::<Value>()
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowlist"), "{err}");
        assert!(backend.last.lock().unwrap().is_none());
    }
}
