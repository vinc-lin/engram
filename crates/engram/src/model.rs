use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Taint {
    #[default]
    Internal,
    ExternalSync,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDoc {
    pub document_id: String,
    pub namespace: String,
    pub key: String,
    pub title: String,
    pub content: String,
    pub author: String,
    #[serde(default)]
    pub taint: Taint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    pub created_at: f64,
    pub updated_at: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewDoc {
    pub key: String,
    pub title: String,
    pub content: String,
    pub author: String,
    #[serde(default)]
    pub taint: Taint,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taint_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Taint::ExternalSync).unwrap(),
            "\"external_sync\""
        );
        assert_eq!(Taint::default(), Taint::Internal);
    }

    #[test]
    fn newdoc_defaults_taint_internal() {
        let n: NewDoc =
            serde_json::from_str(r#"{"key":"k","title":"t","content":"c","author":"alice"}"#)
                .unwrap();
        assert_eq!(n.taint, Taint::Internal);
    }

    #[test]
    fn meta_round_trips_and_defaults_none() {
        // absent → None
        let n: NewDoc =
            serde_json::from_str(r#"{"key":"k","title":"t","content":"c","author":"a"}"#).unwrap();
        assert!(n.meta.is_none());
        // present → preserved as opaque JSON
        let n2: NewDoc = serde_json::from_str(
            r#"{"key":"k","title":"t","content":"c","author":"a","meta":{"category":"fact","importance":0.8}}"#,
        ).unwrap();
        assert_eq!(n2.meta.as_ref().unwrap()["category"], "fact");
    }
}
