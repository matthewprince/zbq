# Lyra in ZBQ

`lyra.js` is Lyra, the karaoke lyrics renderer by matthewprince (MIT, see
`licenses/MIT-lyra.txt`), copied verbatim from its `dist/lyra.js` at commit
`e9e40d5`. Never edit it here: to update, copy a newer `dist/lyra.js` over it.

`lyra-zbq.js` is ZBQ's glue (the `window.zbq` API the lyrics views call) and
`lyra-host.html` the page both are inlined into (`lyrics_qt::lyra_page`). Any
presentation change ZBQ needs goes in the page's stylesheet or the glue, never
in Lyra itself.
