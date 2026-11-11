@echo off
rem Builds the C reference oracle used by the ssc-enc oracle tests.
rem
rem usage: build_oracle.cmd <path to a samsung-ssc-encoder checkout> [output dir]
rem
rem The C sources are compiled straight from the checkout (nothing is copied into this repository) into
rem <output dir> (default: <workspace>\target\c-oracle). Two binaries are produced:
rem   ssc_oracle.exe     /O2 /fp:precise /MD   (the reference the Rust port is compared against)
rem   ssc_oracle_od.exe  /Od /fp:precise /MD   (sanity check: the C output must not depend on optimisation)
rem /MD links the dynamic UCRT, like Rust's x86_64-pc-windows-msvc target, so both sides use the same libm.
rem No /arch flag: SSE2 only, so the compiler never contracts a*b+c into an FMA.
setlocal
if "%~1"=="" (
  echo usage: build_oracle.cmd ^<samsung-ssc-encoder dir^> [output dir]
  exit /b 2
)
set "SRC=%~f1"
set "HERE=%~dp0"
if "%~2"=="" (set "OUT=%HERE%..\..\..\target\c-oracle") else (set "OUT=%~f2")
if not exist "%SRC%\include\ssc_encoder.h" (
  echo not a samsung-ssc-encoder checkout: %SRC%
  exit /b 2
)
rem Find vcvars64.bat unless cl is already on PATH (set VCVARS to override).
where cl >nul 2>nul
if not errorlevel 1 goto have_cl
if defined VCVARS goto call_vcvars
for /d %%v in ("%ProgramFiles%\Microsoft Visual Studio\*") do (
  for /d %%e in ("%%~v\*") do (
    if exist "%%~e\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=%%~e\VC\Auxiliary\Build\vcvars64.bat"
  )
)
if not defined VCVARS (
  echo vcvars64.bat not found; run from a Developer Command Prompt or set VCVARS
  exit /b 2
)
:call_vcvars
call "%VCVARS%" >nul
:have_cl
if not exist "%OUT%\o2" mkdir "%OUT%\o2"
if not exist "%OUT%\od" mkdir "%OUT%\od"

set "FILES=%SRC%\src\ssc_encoder.c %SRC%\src\ssc_enc_analysis.c %SRC%\src\ssc_enc_alloc.c %SRC%\src\ssc_enc_bands.c %SRC%\src\ssc_enc_ec.c %SRC%\src\ssc_enc_energy.c %SRC%\src\ssc_enc_mdct.c %SRC%\src\ssc_enc_pitch.c %SRC%\src\ssc_enc_tables.c"
set "DFILES=%SRC%\tests\decoder\ssc_alloc.c %SRC%\tests\decoder\ssc_bands.c %SRC%\tests\decoder\ssc_cwrs.c %SRC%\tests\decoder\ssc_decoder.c %SRC%\tests\decoder\ssc_ec.c %SRC%\tests\decoder\ssc_energy.c %SRC%\tests\decoder\ssc_synth.c %SRC%\tests\decoder\ssc_tables.c %SRC%\tests\decoder\ssc_twiddle.c"
set "COMMON=/nologo /W3 /fp:precise /MD /D_CRT_SECURE_NO_WARNINGS /DSSC_DEBUG_TELL /I"%SRC%\include" /I"%SRC%\tests\decoder""

cl %COMMON% /O2 /Fo"%OUT%\o2\\" /Fe"%OUT%\ssc_oracle.exe" "%HERE%ssc_oracle.c" %FILES% %DFILES% || exit /b 1
cl %COMMON% /Od /Fo"%OUT%\od\\" /Fe"%OUT%\ssc_oracle_od.exe" "%HERE%ssc_oracle.c" %FILES% %DFILES% || exit /b 1
echo built %OUT%\ssc_oracle.exe and %OUT%\ssc_oracle_od.exe
