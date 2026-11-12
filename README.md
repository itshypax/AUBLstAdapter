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

Spielordner und Config wählen (beides merkt sich der Adapter), optional eine PIN eintragen und "Token anfordern" drücken. Ab dann läuft die Verbindung. "Leitstelle öffnen" öffnet die Leitstelle im Browser und kopiert den Link, "Spiel starten" startet `Em4.exe` mit der Mod.

Fehler stehen unten im Fenster und in `error.log` neben der Exe.

Wer bisher den C#-Adapter benutzt hat, muss nichts neu einstellen: Spielordner und Config liegen unter demselben Registry-Schlüssel (`HKCU\Software\EMLst`).

## Entwickeln

```
cargo test
cargo run
```

`src/adapter.rs` enthält Logdatei, Sync-Datei und die Abfragen an den Server, `src/main.rs` das Fenster (egui). Ein Tag `v*` baut die Exe und hängt sie an ein Release.

Hervorgegangen aus dem C#-Adapter [Floko122/EMLstAdapter](https://github.com/Floko122/EMLstAdapter), Lizenz Apache 2.0.
