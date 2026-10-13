-- Arm "full": plan + reorient + the std/checkpoint self-check before the final answer.
require("arms/lever").install({ plan = true, reorient = true })
require("std/checkpoint").install({ hooks = require("std/hooks") })
