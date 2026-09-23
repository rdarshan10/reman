# Build environment without the Windows SDK installer: VS 2022's MSVC toolset (link.exe, cl.exe,
# CRT) + Windows SDK libs/headers fetched by xwin into ~/.reman/toolchain/winsdk.
# Usage:  . .\msvc-env.ps1; cargo build --release
$vc  = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC'
$msvc = Get-ChildItem "$vc\Tools\MSVC" | Sort-Object Name -Descending | Select-Object -First 1 -ExpandProperty FullName
$sdk = Join-Path $HOME '.reman\toolchain\winsdk\sdk'
$env:VCINSTALLDIR      = "$vc\"
$env:VCToolsInstallDir = "$msvc\"
$env:PATH    = "$msvc\bin\Hostx64\x64;$env:PATH"
$env:LIB     = "$msvc\lib\x64;$sdk\lib\um\x86_64;$sdk\lib\ucrt\x86_64"
$env:INCLUDE = "$msvc\include;$sdk\include\ucrt;$sdk\include\um;$sdk\include\shared"
