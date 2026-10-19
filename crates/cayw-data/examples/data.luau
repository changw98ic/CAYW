local parsed = cayw.call("data.parse", {
    format = "csv",
    text = 'id,note\n001,"hello, world"\n002,雪\n'
}).value
local rows = cayw.call("data.query", {value = parsed, pointer = "/rows"}).value
local selected = cayw.call("data.transform", {
    value = rows,
    operation = "select_rows",
    indices = {1, 0} -- JSON row indices remain zero-based.
}).value
local serialized = cayw.call("data.serialize", {
    format = "csv",
    value = {header = parsed.header, rows = selected}
}).text
local digest = cayw.call("hash.digest", {
    algorithm = "sha256",
    input = {kind = "utf8", value = serialized}
})
return {csv = serialized, digest = digest}
