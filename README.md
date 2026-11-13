# AUBLst Adapter

Läuft auf dem Rechner mit Emergency 4 und verbindet das Spiel mit einer [AUBLst](https://github.com/itshypax/AUBLst)-Leitstelle. Der Adapter liest, was die Mod in die `logfile.txt` des Spiels schreibt (Fahrzeuge, Einsätze, Krankenhäuser, Funkmeldungen), schickt es an den Server und schreibt die Befehle der Leitstelle in die Sync-Datei, die die Mod liest.

Eine einzelne `emlst-adapter.exe`, ohne .NET und ohne Installation.

## Mit der eigenen Mod ausliefern

1. `emlst-adapter.exe` aus dem [neuesten Release](../../releases/latest) laden.
2. `example_config.cfg` als `config.cfg` neben die Exe legen und ausfüllen:
   - `mod_id` (oben und in `request_data`): die ID, unter der das Kartenbild der Mod beim Server liegt.
   - `map_bounds`: bei einer vollen Freeplay-Karte so lassen.
   - `server_path`: die API des Servers, z. B. `https://example.org/backend/api.php`.
   - `sync_path`: die Datei, in die der Adapter Befehle schreibt, relativ zum Spielordner. Ohne Angabe `input.txt`.
   - `initial_data` (optional): wird direkt nach dem Verbinden einmal an `sync` geschickt.
3. Beides zusammen mit der Mod ausliefern.

## Benutzen

Spielordner und Config wählen (beides merkt sich der Adapter), optional eine PIN eintragen und "Code anfordern" drücken. Ab dann läuft die Verbindung. Code und Leitstellen-Link lassen sich kopieren, der Link auch direkt im Browser öffnen. "Spiel starten" startet `Em4.exe` mit der Mod. Wer Spielordner oder Config ändern will, trennt vorher; ein Neustart des Adapters ist dafür nicht nötig.

Unter den Einstellungen stehen Status und Log, beide einklappbar. Zugeklappt zeigt der Status nur, ob alles läuft oder was fehlt, und das Log nur seinen letzten Eintrag. Aufgeklappt gibt es das Adapter-Log und die rohen Zeilen aus der `logfile.txt` des Spiels.

Befehle der Leitstelle holt der Adapter nur ab, solange `Em4.exe` läuft und die Mod sich in dieser Runde gemeldet hat. Was die Leitstelle schickt, während das Spiel zu ist, bleibt auf dem Server liegen und kommt nach dem nächsten Spielstart an. Lehnt der Server dieselben Spieldaten fünfmal ab, verwirft der Adapter sie und schreibt sie ins `error.log`, damit spätere Meldungen wieder durchkommen.

Fehler stehen im Log des Fensters und in `error.log` neben der Exe.

Wer bisher den C#-Adapter benutzt hat, muss nichts neu einstellen: Spielordner und Config liegen unter demselben Registry-Schlüssel (`HKCU\Software\EMLst`).

## Entwickeln

```
cargo test
cargo run
```

`src/adapter.rs` enthält Logdatei, Sync-Datei und die Abfragen an den Server, `src/main.rs` das Fenster (egui). Ein Tag `v*` baut die Exe und hängt sie an ein Release.

Zum Bauen braucht es `rc.exe` aus dem Windows SDK, das der Exe das AUBLst-Symbol gibt.

Hervorgegangen aus dem C#-Adapter [Floko122/EMLstAdapter](https://github.com/Floko122/EMLstAdapter), Lizenz Apache 2.0. Die Icons sind [Phosphor Icons](https://phosphoricons.com) (MIT, siehe `assets/Phosphor-LICENSE.txt`).
