# Temporary helper for the directive-directions branch.
# Bulk transforms tools.rs:
#   1) Replaces every working_dir description block with a reference to one of
#      the WORKING_DIR_DESC* constants.
#   2) Appends an "ALWAYS pass `working_dir`" directive to the top-level
#      description of every tool that accepts a working_dir parameter.
# Uses explicit UTF-8 byte I/O to preserve non-ASCII source bytes.
param([string]$Path = "crates/cargo-mcp/src/tools.rs")

$utf8 = New-Object System.Text.UTF8Encoding $false
$text = [System.IO.File]::ReadAllText($Path, $utf8)

$standardOld = @"
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
"@
$standardNew = @"
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
"@

$metaOld = @"
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml \
                             (or a workspace member). Defaults to the current directory."
"@
$metaNew = @"
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC_METADATA
"@

$diagOld = @"
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory to diagnose. \
                             Defaults to the current directory."
"@
$diagNew = @"
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC_DIAGNOSTIC
"@

$len0 = $text.Length
$text = $text.Replace($standardOld, $standardNew); $len1 = $text.Length
$text = $text.Replace($metaOld,     $metaNew);     $len2 = $text.Length
$text = $text.Replace($diagOld,     $diagNew);     $len3 = $text.Length

Write-Host "standard block replacements   (bytes saved): $($len0 - $len1)"
Write-Host "metadata block replacements   (bytes saved): $($len1 - $len2)"
Write-Host "diagnostic block replacements (bytes saved): $($len2 - $len3)"

$reminder = ' ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server''s own working directory and will usually cause the call to fail.'

$tools = @(
    'cargo_metadata','cargo_check','cargo_build','cargo_test',
    'cargo_clippy','cargo_fmt_check','cargo_fmt','cargo_tree',
    'cargo_doc','cargo_clean','cargo_update','cargo_fix',
    'cargo_add','cargo_remove','cargo_publish','cargo_diagnostic'
)

foreach ($tool in $tools) {
    # Match: `"name": "<tool>"` ... up to the trailing `."` of its top-level
    # description literal, immediately followed by `,` and `"inputSchema"`.
    # Splice the reminder between the `.` and the closing `"`. The reminder
    # already ends with `.`, so we put back only `"` (not `."`).
    $pat = "(?s)(`"name`":\s*`"$tool`",.*?\.)`"(\s*,\s*`"inputSchema`")"
    $rep = "`${1}${reminder}`"`${2}"
    $new = [regex]::Replace($text, $pat, $rep)
    if ($new -eq $text) { Write-Warning "no splice for $tool" }
    $text = $new
}

[System.IO.File]::WriteAllText($Path, $text, $utf8)
Write-Host "Done."
