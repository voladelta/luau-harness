# Extensions

An extension is a Luau module at `.hx/ext/NAME.luau`. Its first line is a comment that says what it does, and
it returns a table of functions:

```lua
-- counts TODO comments per file
return {
	count = function(dir)
		return host.sh(("grep -rc TODO %s | grep -v ':0$'"):format(dir)).out
	end,
}
```

The model writes extensions with `write` and calls them from its code as `ext("NAME").count("src")`. An
extension can call the same functions the model's code can, and what it prints goes into the tool result with what
that code prints. A changed file is loaded again on its next use. Each
session starts with a list of the project's extensions in the system prompt. The tool itself never changes, so
prompt caching keeps working. Commit `.hx/ext/` and the harness grows with the project.

An extension whose name starts with `_` is a helper for other extensions: `ext()` loads it like any other, and
the system prompt does not list it.

## Built in: bytes, text, path, http, web, ocr and exa

Seven extensions come with hx, in `lua/ext/`. `bytes`, `text`, `path` and `http` need no key and are always
listed. `bytes` does in Luau what a model would otherwise reach for Python's standard library to do with binary
data:

| Call | Returns |
|---|---|
| `ext("bytes").hex(data)`, `.unhex(text)` | lower-case hex; and back, taking upper case, a `0x` and spaces |
| `ext("bytes").base64(data)`, `.unbase64(text)` | standard base64 with padding; and back, taking the URL-safe alphabet, missing padding and line breaks |
| `ext("bytes").base58(data)`, `.unbase58(text)` | base58 in the Bitcoin alphabet, which Solana addresses use, each leading zero byte a `1`; and back |
| `ext("bytes").sha256(data)` | the 32-byte digest, raw: `hex(sha256(data))` reads as `sha256sum` prints it |
| `ext("bytes").sha512(data)`, `.sha1(data)`, `.md5(data)` | the 64-, 20- and 16-byte digests, raw, as `sha512sum`, `sha1sum` and `md5sum` print them in hex |
| `ext("bytes").hmac(algo, key, data)` | the HMAC of `data` under `key`, raw, with `algo` one of `"sha256"`, `"sha512"`, `"sha1"` and `"md5"` |
| `ext("bytes").uint(data, at?, width?)`, `.int(...)` | the little-endian integer of `width` bytes from byte `at` (default all of `data` from byte 1), unsigned or two's complement, as exact decimal text of any width: `tonumber` makes it a number when 2^53 is enough, and `bc` computes with it exactly |

`text` and `path` are string work that Luau's patterns make easy to get wrong:

| Call | Returns |
|---|---|
| `ext("text").escape(s)` | `s` with every pattern character escaped, to put literal text inside a pattern |
| `ext("text").replace(s, old, new, n?)` | `s` with `old` replaced by `new`, at most `n` times, and the count; neither has any pattern meaning, so a `.` or a `%` is itself |
| `ext("text").trim(s)`, `.starts(s, prefix)`, `.ends(s, suffix)` | `s` without whitespace at either end; whether `s` begins or ends with the text |
| `ext("text").lines(s)` | the lines of `s`, each without its `\n` or `\r\n`; a final line break adds no empty line |
| `ext("path").join(a, ...)`, `.dirname(p)`, `.basename(p)`, `.ext(p)`, `.normalize(p)` | as Python's `posixpath` has `join`, `dirname`, `basename`, the extension of `splitext` (`".gz"` of `a.tar.gz`, `""` of `.bashrc`) and `normpath` |
| `ext("path").relative(p, from?)` | `p` relative to `from`, by default the working directory, as `posixpath.relpath`; the file system is not read |

`http` makes a request with curl and returns it as data:

| Call | Returns |
|---|---|
| `ext("http").request(url, {method?, headers?, body?, json?, timeout=60})` | `{status, headers, body}`: the status of the last response after redirects, its headers by lower-case name (one named twice is joined with `, `), and its body. Any status is returned, a 404 with the body it came with; only a request that gets no response raises, with curl's message. `json` is a value sent encoded, with `Content-Type: application/json` unless `headers` names one; `body` is sent as it is, and the two do not go together. The method is `GET`, or `POST` with a body |

Of the other three, two, `web` and `ocr`, call DeepSeek's [Search, Fetch](https://api-docs.deepseek.com/guides/search)
and [OCR](https://api-docs.deepseek.com/guides/ocr) APIs with `DEEPSEEK_API_KEY`, whichever model the loop talks
to, so a session on Claude can use them too when that key is set. The system prompt lists them only then. A
project's own `.hx/ext/web.luau` or `.hx/ext/ocr.luau` takes the place of the built-in one.

| Call | Returns |
|---|---|
| `ext("web").search(query, {count?, content?, question?})` | results, best first: `{title, url, snippet, summary, content?, site_name?, date?, rerank_score, authority}`. Printed or returned from `lua`, they read as a numbered list. `query` takes `site:`, `-site:`, `lang:`, `after:`, `before:` and `"phrases"`, up to 128 characters; `count` is 1 to 50 (default 10); `content = true` asks for each page's text |
| `ext("web").fetch(url)` | the page as Markdown, its title first as a heading when the page does not show it |
| `ext("ocr").read(path, purpose?)` | the text of a PDF, Word, PowerPoint or image file, up to 100 MiB and 1,000 pages. A PDF is read with `rich_layout_recognition`, which keeps tables and layout and leaves box tokens in the text; anything else with `text_recognition`. A document of several pages has a `[page N]` line before each |

Requests that fail with 408, 429 or 5xx, or get no answer, are tried three times, Exa's too. The retry loop, the
reading of error bodies and the argument checks are written once, in the helper `_http`, which all three load; a
project's own `.hx/ext/_http.luau` takes its place for all of them. The file for OCR is read by the
host inside the wall, so a file the model cannot read cannot be sent: `ocr.read` of one fails at once, without a
retry. Search and Fetch cost $0.75 per 1,000
requests, OCR $0.225 per 1,000 pages.

A third, `exa`, calls Exa's [Deep Search](https://exa.ai/docs/search/deep-search) with `EXA_API_KEY`, and is listed
only when that key is set. Deep Search plans several searches, reads what they find, searches again where the
evidence is thin, and writes a grounded answer. It is for questions that take research; a single lookup is quicker
and cheaper through `web`.

| Call | Returns |
|---|---|
| `ext("exa").deep(query, objective, {queries?, schema?, count?})` | `{answer, grounding, results, notices, cost}`: `answer` is text, or a table when `schema` is given; `grounding` lists the citations behind each field; `results` are the sources, best first, `{title, url, date?, author?, highlights}`. Printed or returned from `lua`, it reads as the answer and a numbered list of sources. `objective` says which documents should rank first, which to leave out and what facts to pull; `queries` is up to 10 further starting searches; `schema` is an Exa output schema of type `"object"` (at most 10 properties, 2 levels deep); `count` is 1 to 100 sources (default 10). The search type is always `deep` |

Every request asks for highlights, Exa's excerpts of each page that answer the query, not the whole page. Exa's
notices go to the model with the answer; its warnings go to stderr. Deep Search costs $12 per 1,000 requests up to
10 results, and takes 4 to 15 seconds.
