-- Arm "nock": hx without the default std/checkpoint pack (no heads-up, no self-check). This is what "base" meant
-- before 2026-10-14, when the checkpoint became a default pack.
require("std/checkpoint").enabled = false
