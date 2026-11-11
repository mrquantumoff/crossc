@echo off
rem Compiles bthcheck.c against the WDK km headers with MSVC and runs it.
rem Output is frozen in msvc-layout.txt and mirrored by the const asserts in
rem srcth.rs (checked against the bindgen types on every driver build).
setlocal
call "%ProgramFiles%\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat" >nul || exit /b 1
set K=%ProgramFiles(x86)%\Windows Kits\10\Include\10.0.26100.0
cl /nologo /W3 /I"%K%\km" /I"%K%\shared" /I"%K%\km\crt" /I"%K%\ucrt" /Fe:"%TEMP%\bthcheck.exe" /Fo:"%TEMP%\bthcheck.obj" "%~dp0bthcheck.c" /link /subsystem:console || exit /b 1
"%TEMP%\bthcheck.exe"
