//! Local JSON schemas shared by stored facets and executable plugins.

use serde_json::Value;
use crate::error::{Error, Result};

pub(crate) fn compile(schema: &Value) -> Result<jsonschema::Validator> {
    guard_schema(schema)?;
    jsonschema::validator_for(schema)
        .map_err(|error| Error::BadRequest(format!("invalid schema: {error}")))
}

/// A facet schema is caller-supplied data that the validator later walks, so
/// a `$ref` in it is a request for the core to go and resolve something. Only
/// refs into the document itself are allowed; anything else is refused here,
/// at registration, where there is a human to tell.
fn guard_schema(schema: &Value) -> Result<()> {
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(key.as_str(), "$ref" | "$recursiveRef" | "$dynamicRef") {
                    let target = value.as_str().ok_or_else(|| {
                        Error::BadRequest(format!("schema {key} must be a string"))
                    })?;
                    if !target.starts_with('#') {
                        return Err(Error::BadRequest(format!(
                            "schema {key} {target:?} points outside the document; \
                             only local refs such as \"#/$defs/name\" resolve"
                        )));
                    }
                }
                guard_schema(value)?;
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(guard_schema),
        _ => Ok(()),
    }
}
