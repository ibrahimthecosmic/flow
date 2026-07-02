// Copyright 2018-2026 the Deno authors. MIT license.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct RuntimeOtelExtraAttributes(
  pub HashMap<opentelemetry::Key, opentelemetry::Value>,
);
