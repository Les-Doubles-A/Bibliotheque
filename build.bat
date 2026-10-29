@echo off
cargo build --release --offline
if not exist dist mkdir dist
copy /Y "target\release\Bibliotheque.exe" "dist\Bibliotheque.exe" >nul
echo Build termine : dist\Bibliotheque.exe
