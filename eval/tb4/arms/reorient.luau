-- Arm "reorient": when progress stalls (repeated error, no test gain, no file changes), ask the model to re-orient.
require("arms/lever").install({ reorient = true })
