/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;

use pyrefly_config::config::BaselineField;
use pyrefly_config::error_kind::Severity;
use pyrefly_util::absolutize::Absolutize;
use pyrefly_util::prelude::SliceExt;
use serde::Deserialize;
use serde::Serialize;

use crate::error::error::BaselineStatus;
use crate::error::error::Error;

pub(crate) fn severity_to_str(severity: Severity) -> String {
    match severity {
        Severity::Ignore => "ignore".to_owned(),
        Severity::Info => "info".to_owned(),
        Severity::Warn => "warn".to_owned(),
        Severity::Error => "error".to_owned(),
    }
}

fn default_severity() -> String {
    "error".to_owned()
}

/// Legacy error structure in Pyre1. Needs to be consistent with the following file:
/// <https://www.internalfb.com/code/fbsource/fbcode/tools/pyre/facebook/arc/lib/error.rs>
///
/// Used to serialize errors in a Pyre1-compatible format.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct LegacyError {
    line: usize,
    pub column: usize,
    stop_line: usize,
    stop_column: usize,
    pub path: String,
    /// This field is no longer used in Pyrefly. It is kept here for Pyre1 backward compatibility.
    code: i32,
    /// The kebab-case name of the error kind.
    pub name: String,
    description: String,
    concise_description: String,
    /// This field is not part of Pyre1 error format. But it's useful for Pyrefly clients
    #[serde(default = "default_severity")]
    severity: String,
    /// Whether the error matched a configured baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baselined: Option<bool>,
    /// Optional notebook cell number for errors in notebook files
    #[serde(skip_serializing_if = "Option::is_none")]
    cell: Option<usize>,
}

impl LegacyError {
    pub fn from_error(relative_to: &Path, error: &Error) -> Self {
        let error_range = error.display_range();
        let error_path = error.path().as_path();
        Self {
            line: error_range.start.line_within_cell().get() as usize,
            column: error_range.start.column().get() as usize,
            stop_line: error_range.end.line_within_cell().get() as usize,
            stop_column: error_range.end.column().get() as usize,
            cell: error_range.start.cell().map(|cell| cell.get() as usize),
            path: error_path
                .relativize_from(relative_to)
                .to_string_lossy()
                .replace('\\', "/"), // Normalize Windows backslashes so baseline files are consistent across platforms
            // -2 is chosen because it's an unused error code in Pyre1
            code: -2, // TODO: replace this dummy value
            name: error.error_kind().to_name().to_owned(),
            description: error.msg(),
            concise_description: error.msg_header().to_owned(),
            severity: severity_to_str(error.severity()),
            baselined: match error.baseline_status() {
                BaselineStatus::NotConfigured => None,
                BaselineStatus::NotCompared | BaselineStatus::Unmatched => Some(false),
                BaselineStatus::Matched => Some(true),
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct LegacyErrors {
    pub errors: Vec<LegacyError>,
}

impl LegacyErrors {
    pub fn from_errors(relative_to: &Path, errors: &[Error]) -> Self {
        Self {
            errors: errors.map(|e| LegacyError::from_error(relative_to, e)),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct BaselineError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The kebab-case name of the error kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concise_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    /// Optional notebook cell number for errors in notebook files
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<usize>,
}

fn includes_baseline_field(baseline_fields: &[BaselineField], field: BaselineField) -> bool {
    baseline_fields.contains(&field)
}

impl BaselineError {
    fn from_error(relative_to: &Path, error: &Error, baseline_fields: &[BaselineField]) -> Self {
        let error_range = error.display_range();
        let error_path = error.path().as_path();
        Self {
            line: includes_baseline_field(baseline_fields, BaselineField::Line)
                .then(|| error_range.start.line_within_cell().get() as usize),
            column: includes_baseline_field(baseline_fields, BaselineField::Column)
                .then(|| error_range.start.column().get() as usize),
            cell: includes_baseline_field(baseline_fields, BaselineField::Cell)
                .then(|| error_range.start.cell().map(|cell| cell.get() as usize))
                .flatten(),
            path: includes_baseline_field(baseline_fields, BaselineField::Path).then(|| {
                error_path
                    .relativize_from(relative_to)
                    .to_string_lossy()
                    .replace('\\', "/")
            }),
            name: includes_baseline_field(baseline_fields, BaselineField::Name)
                .then(|| error.error_kind().to_name().to_owned()),
            concise_description: includes_baseline_field(
                baseline_fields,
                BaselineField::ConciseDescription,
            )
            .then(|| error.msg_header().to_owned()),
            severity: includes_baseline_field(baseline_fields, BaselineField::Severity)
                .then(|| severity_to_str(error.severity())),
        }
    }

    /// Remove fields that should not be written under the configured baseline format.
    pub(crate) fn retain_fields(&mut self, baseline_fields: &[BaselineField]) {
        if !includes_baseline_field(baseline_fields, BaselineField::Line) {
            self.line = None;
        }
        if !includes_baseline_field(baseline_fields, BaselineField::Column) {
            self.column = None;
        }
        if !includes_baseline_field(baseline_fields, BaselineField::Path) {
            self.path = None;
        }
        if !includes_baseline_field(baseline_fields, BaselineField::Name) {
            self.name = None;
        }
        if !includes_baseline_field(baseline_fields, BaselineField::ConciseDescription) {
            self.concise_description = None;
        }
        if !includes_baseline_field(baseline_fields, BaselineField::Severity) {
            self.severity = None;
        }
        if !includes_baseline_field(baseline_fields, BaselineField::Cell) {
            self.cell = None;
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct BaselineErrors {
    pub errors: Vec<BaselineError>,
}

impl BaselineErrors {
    pub fn from_errors(
        relative_to: &Path,
        errors: &[Error],
        baseline_fields: &[BaselineField],
    ) -> Self {
        Self {
            errors: errors.map(|e| BaselineError::from_error(relative_to, e, baseline_fields)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use pyrefly_config::config::DEFAULT_BASELINE_FIELDS;
    use pyrefly_python::module::Module;
    use pyrefly_python::module_name::ModuleName;
    use pyrefly_python::module_path::ModulePath;
    use ruff_text_size::TextRange;
    use ruff_text_size::TextSize;

    use super::*;
    use crate::config::error_kind::ErrorKind;

    #[test]
    fn test_baseline_error_retains_configured_fields() {
        let mut error: BaselineError = serde_json::from_value(serde_json::json!({
            "line": 4,
            "column": 8,
            "path": "test.py",
            "name": "bad-return",
            "concise_description": "test",
            "severity": "error",
            "cell": 2
        }))
        .unwrap();
        error.retain_fields(&[
            BaselineField::Path,
            BaselineField::Name,
            BaselineField::ConciseDescription,
        ]);

        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({
                "path": "test.py",
                "name": "bad-return",
                "concise_description": "test"
            })
        );
    }

    #[test]
    fn test_baseline_error_retains_default_fields() {
        let mut error: BaselineError = serde_json::from_value(serde_json::json!({
            "line": 4,
            "column": 8,
            "path": "test.py",
            "name": "bad-return",
            "concise_description": "test",
            "severity": "error",
            "cell": 2
        }))
        .unwrap();
        error.retain_fields(DEFAULT_BASELINE_FIELDS);

        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({
                "column": 8,
                "path": "test.py",
                "name": "bad-return"
            })
        );
    }

    #[test]
    fn test_relativize_when_error_is_not_under_relative_to() {
        let module = Module::new(
            ModuleName::from_str("foo"),
            ModulePath::filesystem(PathBuf::from("/repo/libs/foo.py")),
            Arc::new("x = 1\n".to_owned()),
        );
        let error = Error::new(
            module,
            TextRange::new(TextSize::new(0), TextSize::new(1)),
            "err".to_owned(),
            Vec::new(),
            ErrorKind::BadAssignment,
        );
        let legacy = LegacyError::from_error(Path::new("/repo/src"), &error);
        assert_eq!(legacy.path, "../libs/foo.py");
    }

    #[test]
    fn test_baseline_provenance_is_optional() {
        let module = Module::new(
            ModuleName::from_str("foo"),
            ModulePath::filesystem(PathBuf::from("/repo/foo.py")),
            Arc::new("x = 1\n".to_owned()),
        );
        let error = Error::new(
            module,
            TextRange::new(TextSize::new(0), TextSize::new(1)),
            "err".to_owned(),
            Vec::new(),
            ErrorKind::BadAssignment,
        );

        let without_baseline =
            serde_json::to_value(LegacyError::from_error(Path::new("/repo"), &error)).unwrap();
        assert!(without_baseline.get("baselined").is_none());

        let matched = error.clone().with_baseline_status(BaselineStatus::Matched);
        assert_eq!(
            serde_json::to_value(LegacyError::from_error(Path::new("/repo"), &matched)).unwrap()["baselined"],
            true
        );

        let not_compared = error.with_baseline_status(BaselineStatus::NotCompared);
        assert_eq!(
            serde_json::to_value(LegacyError::from_error(Path::new("/repo"), &not_compared))
                .unwrap()["baselined"],
            false
        );
    }
}
