param([Parameter(Mandatory=$true)][string]$CargoHome)
$ErrorActionPreference = 'Stop'
$taskSourceRoot = Split-Path -Parent $PSScriptRoot
$taskCacheRoot = [IO.Path]::GetFullPath($CargoHome)
$taskUserRoot = [Environment]::GetFolderPath('UserProfile').TrimEnd('\') + '\'
if ($taskCacheRoot.StartsWith($taskUserRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Use a neutral Cargo cache outside the user profile, for example C:\BuildCache\peri-cloud. Native C dependencies may embed their source paths.'
}
$env:CARGO_HOME = $taskCacheRoot
$env:CARGO_INCREMENTAL = '0'
$env:CARGO_ENCODED_RUSTFLAGS = @("--remap-path-prefix=$taskSourceRoot=/src", "--remap-path-prefix=$taskCacheRoot=/cargo") -join [char]31
cargo build --manifest-path (Join-Path $taskSourceRoot 'Cargo.toml') --release --locked -p peri-cloud --bin peri-cloud-host -p peri-executor --bin peri-executor
if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
# Ship optimized .exe files only. Do not distribute PDBs, configuration or state.