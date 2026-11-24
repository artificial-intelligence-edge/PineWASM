//! Source for the displaced-table identity regression.

pub const SOURCE: &str = r#"//@version=6
indicator("Displaced table collection identity", overlay=false)
var table a = table.new(position.top_right, 1, 1)
var array<table> refs = array.new<table>()
if barstate.isfirst
    array.push(refs, a)
    table b = table.new(position.top_right, 1, 1)
    array.push(refs, b)
    table c = table.new(position.top_right, 1, 1)
    array.push(refs, c)
    runtime.error("IDENTITY | a-na=" + str.tostring(na(a)) + ";b-na=" + str.tostring(na(b)) + ";c-na=" + str.tostring(na(c)) + ";index-na=" + str.tostring(array.indexof(refs, na)) + ";index-a=" + str.tostring(array.indexof(refs, a)) + ";index-b=" + str.tostring(array.indexof(refs, b)) + ";index-c=" + str.tostring(array.indexof(refs, c)) + ";includes-na=" + str.tostring(array.includes(refs, na)))
plot(0)
"#;
