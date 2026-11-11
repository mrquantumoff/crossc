@echo off
rem Compiles acxcheck.c against the WDK km / KMDF 1.33 / ACX 1.1 headers with
rem MSVC and runs it. Output is frozen in msvc-layout.txt and mirrored by the
rem const asserts in src\acx.rs (checked against the bindgen types on every
rem driver build).
setlocal
call "%ProgramFiles%\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
set K=%ProgramFiles(x86)%\Windows Kits\10\Include\10.0.26100.0
set W=%ProgramFiles(x86)%\Windows Kits\10\Include\wdf\kmdf\1.33
cl /nologo /W3 /I"%K%\km" /I"%K%\km\crt" /I"%K%\shared" /I"%K%\um" /I"%K%\ucrt" /I"%W%" /I"%K%\km\acx\km\1.1" /Fe:"%TEMP%\acxcheck.exe" /Fo:"%TEMP%\acxcheck.obj" "%~dp0acxcheck.c" /link /subsystem:console || exit /b 1
"%TEMP%\acxcheck.exe"
