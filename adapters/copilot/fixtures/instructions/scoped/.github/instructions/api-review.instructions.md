---
description: API handler review guidance
applyTo:
  - "crates/*/src/**/*.rs"
excludeAgent: false
customRouting: reviewer
---

# API Review

When reviewing API handlers, check that domain behavior remains outside command
parsing code and that filesystem paths stay as structured path values.
