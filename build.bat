@echo off
setlocal
set "RUSTUP_HOME=D:\rust\rustup"
set "CARGO_HOME=D:\rust\cargo"
set "PATH=D:\rust\cargo\bin;D:\tools\zig-x86_64-windows-0.17.1;%PATH%"
call "D:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 exit /b 1
cargo %*
