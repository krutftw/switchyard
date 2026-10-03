#Requires -Version 5.1
<#
.SYNOPSIS
Package explicit Windows x64 Switchya preview binaries without building or running them.
.DESCRIPTION
Uses an allowlist, fixed ZIP entry timestamps and ordinal entry order. Identical
inputs on the same PowerShell/.NET runtime produce identical outputs. Existing
release files are never replaced. Failed runs retain their new .partial files.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$CliPath,
    [Parameter(Mandatory = $true)][string]$DesktopPath,
    [Parameter(Mandatory = $true)][string]$ReleaseNotesPath,
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$')][string]$SourceRevision,
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+-preview\.[0-9]+$')]
    [string]$Version = '0.1.0-preview.1',
    [string]$OutputDirectory = 'E:\switchya-artifacts'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$outputRoot = [IO.Path]::GetFullPath($OutputDirectory)
$stem = "switchya-$Version-windows-x86_64-portable"
$archiveName = "$stem.zip"
$manifestName = "$stem.manifest.json"
$archivePath = Join-Path $outputRoot $archiveName
$manifestPath = Join-Path $outputRoot $manifestName
$checksumPath = Join-Path $outputRoot "$stem.sha256"
$utf8 = New-Object System.Text.UTF8Encoding($false)
$inputs = New-Object 'System.Collections.Generic.List[object]'

function Resolve-RegularFile([string]$Path, [string]$ExpectedName = '') {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item -isnot [IO.FileInfo] -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Expected a regular file, not a directory or reparse point: $Path"
    }
    if ($ExpectedName -and $item.Name -ine $ExpectedName) {
        throw "Expected the binary named $ExpectedName; received $($item.Name)."
    }
    if ($item.Length -eq 0) { throw "Input is empty: $Path" }
    return $item.FullName
}

function Get-StreamHash([IO.Stream]$Stream) {
    $hash = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($hash.ComputeHash($Stream))).Replace('-', '').ToLowerInvariant() }
    finally { $hash.Dispose() }
}

function Assert-WindowsX64([IO.Stream]$Stream, [string]$Name) {
    $reader = New-Object IO.BinaryReader($Stream, [Text.Encoding]::UTF8, $true)
    try {
        if ($Stream.Length -lt 128 -or $reader.ReadUInt16() -ne 0x5a4d) { throw "$Name is not a PE executable." }
        $Stream.Position = 0x3c
        $offset = $reader.ReadInt32()
        if ($offset -lt 64 -or $offset -gt ($Stream.Length - 26)) { throw "$Name has an invalid PE header." }
        $Stream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x8664) {
            throw "$Name must be a Windows x86_64 executable."
        }
        $Stream.Position = $offset + 22
        if (($reader.ReadUInt16() -band 0x0002) -eq 0 -or $reader.ReadUInt16() -ne 0x020b) {
            throw "$Name must be a PE32+ executable image."
        }
    }
    finally { $reader.Dispose(); $Stream.Position = 0 }
}

function Add-InputFile([string]$RelativePath, [string]$Path, [bool]$Executable = $false) {
    $resolved = Resolve-RegularFile $Path
    # Retain the same read-only handle through packaging; deny concurrent writes.
    $stream = [IO.File]::Open($resolved, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($Executable) { Assert-WindowsX64 $stream $RelativePath }
        $hash = Get-StreamHash $stream
        $stream.Position = 0
        $inputs.Add([pscustomobject]@{ Path = "$stem/$RelativePath"; Stream = $stream; Size = $stream.Length; Sha256 = $hash })
    }
    catch { $stream.Dispose(); throw }
}

function Write-NewUtf8([string]$Path, [string]$Text) {
    $stream = [IO.File]::Open($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $bytes = $utf8.GetBytes(($Text -replace "`r`n", "`n"))
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    }
    finally { $stream.Dispose() }
}

function Assert-OutputPath([string]$Path) {
    $full = [IO.Path]::GetFullPath($Path)
    $prefix = $outputRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'An output path escaped the explicitly selected artifact directory.'
    }
}

try {
    foreach ($path in @($archivePath, $manifestPath, $checksumPath)) {
        Assert-OutputPath $path
        if (Test-Path -LiteralPath $path) { throw "Refusing to replace an existing release file: $path" }
    }
    Add-InputFile 'switchya.exe' (Resolve-RegularFile $CliPath 'switchya.exe') $true
    Add-InputFile 'switchya-desktop.exe' (Resolve-RegularFile $DesktopPath 'switchya-desktop.exe') $true
    Add-InputFile 'RELEASE-NOTES.md' $ReleaseNotesPath
    # Deliberately no recursive copy, gateway configuration, user state, tests,
    # environment files, launch-info files, or credential directories.
    foreach ($relative in @(
        'LICENSE', 'NOTICE',
        'docs/SWITCHYA.md', 'docs/ACCOUNTS.md', 'docs/AGENT-ADAPTERS.md',
        'docs/APP-API.md', 'docs/APP-ROADMAP.md', 'docs/PLATFORM-VERIFICATION.md', 'desktop/README.md',
        'app-ui/vendor/LICENSES.txt', 'app-ui/fonts/Archivo-OFL.txt', 'app-ui/fonts/JetBrainsMono-OFL.txt',
        'ui/vendor/LICENSES.txt', 'ui/fonts/Archivo-OFL.txt', 'ui/fonts/JetBrainsMono-OFL.txt'
    )) {
        Add-InputFile $relative (Join-Path $repoRoot $relative)
    }
    $readme = @'
# Switchya {{VERSION}} - Windows x64 portable preview

This preview targets Windows 10/11 x64 and requires the
[Microsoft Visual C++ v14 x64 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170).
The runtime is not bundled. Extract the whole ZIP to a folder you can write to,
then double-click `switchya-desktop.exe`. This preview has no installer or code signing.
The desktop shell also needs the Microsoft Edge WebView2 Runtime; see the
[official Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/#webview2).

For the browser workspace, open PowerShell in this folder and run:

```powershell
.\switchya.exe serve
```

Open the local launch URL printed in that terminal. Keep the terminal running;
Ctrl+C stops the host. Keep its credential-bearing URL private.

Switchya creates its initial configuration and session storage under
`%LOCALAPPDATA%\Switchya`. This portable archive does not make user data portable:
configuration, saved sessions and account homes stay in the application-data
directory unless explicit paths are supplied. No API keys or account sign-ins
are included. Use provider setup for the built-in agent; install official Codex
or Claude CLIs separately for their account workflows.

For terminal work, close the desktop/browser host first, or connect to its
explicitly exported private launch-info file as explained in the user guide:

```powershell
.\switchya.exe models
.\switchya.exe chat --project 'C:\projects\my-app' --model 'MODEL_ID'
.\switchya.exe sessions
```

Use a real model ID returned by `models`. File edits and commands require an
explicit decision; approved commands run with your current OS permissions.
Review output and actual checks before treating a completed task as verified.

Read [RELEASE-NOTES.md](RELEASE-NOTES.md) for this preview's verification limits,
and [the user guide](docs/SWITCHYA.md) for browser/CLI handoff, providers, accounts,
interruptions and recovery. Codex supports structured app runs; Claude launches
in an external terminal and that terminal session is not tracked by Switchya.

## Switchyard Gateway

The separately released [Switchyard Gateway](https://github.com/krutftw/switchyard)
is a different product. Its release status does not establish Switchya platform
support. This ZIP contains Switchya only, with its embedded local gateway runtime.

The adjacent `.manifest.json` records every packaged file's SHA256 and the ZIP
hash. The `.sha256` file covers the ZIP and manifest. A supplied source revision
is recorded for traceability; this packager does not verify build provenance.
'@
    $readmeBytes = $utf8.GetBytes(($readme.Replace('{{VERSION}}', $Version) -replace "`r`n", "`n") + "`n")
    $readmeStream = New-Object IO.MemoryStream(,$readmeBytes)
    $inputs.Add([pscustomobject]@{ Path = "$stem/README.md"; Stream = $readmeStream; Size = $readmeBytes.Length; Sha256 = (Get-StreamHash $readmeStream) })
    $readmeStream.Position = 0
    [string[]]$entryNames = @($inputs | ForEach-Object { $_.Path })
    [Array]::Sort($entryNames, [StringComparer]::Ordinal)
    $byName = @{}
    foreach ($inputFile in $inputs) { $byName.Add($inputFile.Path, $inputFile) }
    if ($byName.Count -ne $entryNames.Length) { throw 'Duplicate archive entry.' }

    [void][IO.Directory]::CreateDirectory($outputRoot)
    if ((Get-Item -LiteralPath $outputRoot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'The artifact directory must not be a reparse point.'
    }
    $partialStem = Join-Path $outputRoot ('.switchya-package-' + [Guid]::NewGuid().ToString('N'))
    $partialArchive = "$partialStem.zip.partial"
    $partialManifest = "$partialStem.manifest.partial"
    $partialChecksum = "$partialStem.sha256.partial"
    $zipStream = [IO.File]::Open($partialArchive, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try {
        $zip = New-Object IO.Compression.ZipArchive($zipStream, [IO.Compression.ZipArchiveMode]::Create, $true)
        try {
            foreach ($name in $entryNames) {
                $item = $byName[$name]
                $entry = $zip.CreateEntry($name, [IO.Compression.CompressionLevel]::Optimal)
                $entry.LastWriteTime = [DateTimeOffset]::new(1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
                $entry.ExternalAttributes = 0
                $destination = $entry.Open()
                try { $item.Stream.Position = 0; $item.Stream.CopyTo($destination) }
                finally { $destination.Dispose() }
            }
        }
        finally { $zip.Dispose() }
        $zipStream.Flush($true)
        $zipStream.Position = 0
        $verification = New-Object IO.Compression.ZipArchive($zipStream, [IO.Compression.ZipArchiveMode]::Read, $true)
        try {
            if ($verification.Entries.Count -ne $entryNames.Length) { throw 'Archive entry count changed.' }
            foreach ($entry in $verification.Entries) {
                if (-not $byName.ContainsKey($entry.FullName)) { throw "Unexpected archive entry: $($entry.FullName)" }
                $source = $byName[$entry.FullName]
                $entryStream = $entry.Open()
                try {
                    if ($entry.Length -ne $source.Size -or (Get-StreamHash $entryStream) -ne $source.Sha256) {
                        throw "Archive verification failed: $($entry.FullName)"
                    }
                }
                finally { $entryStream.Dispose() }
            }
        }
        finally { $verification.Dispose() }
        $zipStream.Position = 0
        $archiveHash = Get-StreamHash $zipStream
        $archiveSize = $zipStream.Length
    }
    finally { $zipStream.Dispose() }

    $manifest = [ordered]@{
        schema_version = 1
        product = 'Switchya'
        version = $Version
        target = 'x86_64-pc-windows-msvc'
        source_revision_supplied = $SourceRevision.ToLowerInvariant()
        build_provenance_verified_by_packager = $false
        archive = [ordered]@{ file = $archiveName; bytes = $archiveSize; sha256 = $archiveHash }
        files = @($entryNames | ForEach-Object {
            $item = $byName[$_]
            [ordered]@{ path = $item.Path; bytes = $item.Size; sha256 = $item.Sha256 }
        })
    }
    Write-NewUtf8 $partialManifest (($manifest | ConvertTo-Json -Depth 6) + "`n")
    $manifestHash = (Get-FileHash -LiteralPath $partialManifest -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-NewUtf8 $partialChecksum "$archiveHash  $archiveName`n$manifestHash  $manifestName`n"
    foreach ($path in @($partialArchive, $partialManifest, $partialChecksum, $archivePath, $manifestPath, $checksumPath)) {
        Assert-OutputPath $path
    }
    # The two-argument File.Move overload fails if the destination exists.
    [IO.File]::Move($partialArchive, $archivePath)
    [IO.File]::Move($partialManifest, $manifestPath)
    [IO.File]::Move($partialChecksum, $checksumPath)
    [pscustomobject]@{ Archive = $archivePath; Manifest = $manifestPath; Checksums = $checksumPath; SHA256 = $archiveHash }
}
catch {
    Write-Warning 'Packaging did not complete. Existing releases were not replaced; any newly created .partial files are retained for inspection.'
    throw
}
finally {
    foreach ($item in $inputs) { $item.Stream.Dispose() }
}
