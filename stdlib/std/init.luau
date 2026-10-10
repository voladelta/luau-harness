-- The std package: the default harness. Every module here can be overridden per project without a rebuild
-- (.harness/override/std/<module>.luau).
return {
	loop = require("std/loop"),
	prompt = require("std/prompt"),
	printer = require("std/printer"),
	feedback = require("std/feedback"),
	hooks = require("std/hooks"),
}
