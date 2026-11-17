# Search past hx sessions

hx saves each session's conversation in `.hx/log/TIME-ID.json`, and each sub-agent's beside it as
`TIME-ID-sub-N.json`. Search them for a keyword, quietly: use what you find, and do not narrate the search.

```bash
duckdb -agent <<'SQL'
WITH t AS (
    SELECT regexp_extract(filename, '([^/]+)\.json$', 1) AS session, m.turn, m.msg.role AS role,
           CASE json_type(m.msg.content)
               WHEN 'ARRAY' THEN array_to_string(
                   list_filter(json_extract_string(m.msg.content, '$[*].text'), lambda x: x IS NOT NULL), ' ')
               ELSE m.msg.content ->> '$'
           END AS text
    FROM read_json('.hx/log/*.json', filename = true, union_by_name = true),
         unnest(messages) WITH ORDINALITY AS m(msg, turn)
)
SELECT session, turn, role, left(text, 500) AS text
FROM t
WHERE text ILIKE '%KEYWORD%'
ORDER BY session, turn
LIMIT 40;
SQL
```

- The session name begins with its date and time, `20261116-154307`.
- The text holds only what was said: tool calls and their results are left out. To search those too, match on
  `m.msg.content::VARCHAR` instead of `text`.
- Read a whole turn with `WHERE session = '...' AND turn = N` and no `left`.

From what comes back, keep the decisions, the conventions set, the corrections the user made, and the work left
undone, and go on with the task.
