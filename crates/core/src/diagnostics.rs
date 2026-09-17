//! Safe diagnostic categories and static operation stages; never format raw causes.
#[derive(Debug)]
pub struct Stage(pub &'static str);
impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Stage {}

#[derive(Debug)]
pub struct HttpStatus(pub u16);
impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Upstream HTTP {}", self.0)
    }
}
impl std::error::Error for HttpStatus {}

#[derive(Debug)]
pub struct Details {
    pub stage: &'static str,
    pub reason: &'static str,
    pub upstream_status: Option<u16>,
}

pub fn describe(error: &anyhow::Error) -> Details {
    let reason = if error.downcast_ref::<HttpStatus>().is_some() {
        "upstream_http"
    } else if let Some(io) = error.downcast_ref::<std::io::Error>() {
        match io.kind() {
            std::io::ErrorKind::NotFound => "file_not_found",
            std::io::ErrorKind::PermissionDenied => "io_permission_denied",
            std::io::ErrorKind::TimedOut => "io_timeout",
            _ => "io_failure",
        }
    } else if error.downcast_ref::<serde_json::Error>().is_some() {
        "json_decode"
    } else if error.downcast_ref::<toml::de::Error>().is_some() {
        "toml_decode"
    } else {
        "validation_or_internal"
    };
    Details {
        stage: error.downcast_ref::<Stage>().map_or("unspecified", |s| s.0),
        reason,
        upstream_status: error.downcast_ref::<HttpStatus>().map(|s| s.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_error_contents_are_not_diagnostics() {
        let error = anyhow::anyhow!("SECRET_TOKEN private input roster contents")
            .context(Stage("snapshot_decode"));
        let details = describe(&error);
        assert_eq!(details.stage, "snapshot_decode");
        assert_eq!(details.reason, "validation_or_internal");
        assert!(!format!("{details:?}").contains("SECRET"));
    }
}
