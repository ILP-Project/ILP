use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Fixture {
    pub schema_version: u32,
    pub request: FixtureRequest,
    pub response: FixtureResponse,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct FixtureRequest {
    pub method: String,
    pub url: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body_base64: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FixtureResponse {
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub body_base64: String,
}

impl Fixture {
    pub fn decode_body(&self) -> Result<Vec<u8>, String> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&self.response.body_base64)
            .map_err(|e| format!("fixture response body 不是有效 base64: {e}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!("不支持的 fixture schema: {}", self.schema_version));
        }
        if self.request.url.contains("cookie") || self.notes.contains("secret") {
            return Err("fixture 疑似包含敏感信息".into());
        }
        let _ = self.decode_body()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_decodes_fixture() {
        let fixture: Fixture = serde_json::from_str(include_str!("../fixtures/example-http.json")).expect("解析 fixture");
        fixture.validate().expect("fixture 应有效");
        assert_eq!(fixture.decode_body().expect("解码"), b"{\"chapters\":[]}");
        assert_eq!(fixture.request.method, "GET");
    }

    #[test]
    fn rejects_unknown_schema() {
        let fixture = Fixture {
            schema_version: 2,
            request: FixtureRequest { method: "GET".into(), url: "https://example.com".into(), headers: Default::default(), body_base64: None },
            response: FixtureResponse { status: 200, headers: Default::default(), body_base64: "".into() },
            notes: String::new(),
        };
        assert!(fixture.validate().is_err());
    }
}
