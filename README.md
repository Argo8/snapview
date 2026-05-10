# snapview

Ultra-brzi photo viewer u Rustu. Frameless prozor, sve preko shortcuta.

---

## Kako dobiti gotov `snapview.exe` preko GitHub Actions

Slijedi 6 kratkih koraka. Nakon toga, svaki put kad pushaš promjenu, novi exe se automatski builda u oblaku.

### 1. Napravi GitHub account (ako ga već nemaš)

https://github.com/signup — besplatno, 30 sekundi.

### 2. Napravi novi repository

https://github.com/new
- Name: `snapview` (ili kako god želiš)
- Public ili Private — svejedno, GitHub Actions je besplatan i za privatne repoe (do 2000 min/mj)
- **Ne** dodavaj README, .gitignore ni LICENSE — već su u zipu
- Klikni **Create repository**

### 3. Uploadaj fajlove

Najlakše bez instalacije gita: na novoj prazno-repo stranici klikni **"uploading an existing file"** link, pa drag-and-drop **sve** iz zipa (uključujući `.github` folder — pazi da nije skriven!).

Ako Windows Explorer skriva `.github` folder:
- View → Show → kvačica na "Hidden items"
- Ili u browseru klikni "choose your files" pa Ctrl+A izaberi sve

Klikni **Commit changes** dolje.

### 4. Pričekaj build (~5 min)

Idi na karticu **Actions** u repou. Vidjet ćeš workflow "Build snapview.exe" kako se vrti. Kad bude zelena kvačica:

### 5. Skini exe

Klikni na završeni run → na dnu stranice vidjet ćeš **Artifacts** → **`snapview-windows`** → klikni za download. Dobit ćeš zip s `snapview.exe` unutra.

### 6. Pokreni

Raspakiraj, dvoklik na `snapview.exe`. Ako iskoči SmartScreen → "More info" → "Run anyway" (jer exe nije digitalno potpisan).

---

## Bonus: automatski release s gotovim exe-om

Kad si zadovoljan kodom, u repou:

```
Releases (desno gore) → Create a new release → Choose a tag → "v1.0.0" → Create new tag → Publish release
```

Workflow će automatski attach-ati `snapview.exe` na release i bit će dostupan kao public download link. Tagovi koji počinju s `v` (npr. `v1.0.0`, `v0.2.1`) triggeraju release.

---

## Manualno triggeranje builda

Actions kartica → "Build snapview.exe" → "Run workflow" gumb → biraš branch → Run.

---

## Shortcuts u snapview-u

| Tipka | Akcija |
|---|---|
| **→** ili scroll dolje | Sljedeća slika |
| **←** ili scroll gore | Prethodna slika |
| **Q** / **W** | Rotiraj lijevo / desno |
| **Space** | Označi favorit (★) |
| **F** | Filter favorita + kopiranje u drugi folder |
| **Ctrl+O** | Otvori folder |
| **F11** ili dvoklik | Maximize |
| **Esc** | Zatvori |
| Desni klik | Cijeli meni s svime |
| Drag prozora | Lijevi klik + povuci |
| Drag&drop | Baci folder ili sliku u prozor |

## Workflow s favoritima

1. Otvori folder pun slika (Ctrl+O ili drag&drop)
2. Listaj strelicama, **Space** označava one koje želiš
3. **F** otvara filter prozor s svim označenima
4. Otkvači one koje na kraju ne želiš
5. "Copy N selected to folder…" → bira destinaciju → gotovo

Favoriti se spremaju u `.favorites.txt` u tom folderu (plain text, lako za debug i preživi premještanje).

## Podržani formati

JPG, PNG, BMP, GIF, WebP, TIFF.
HEIC i RAW nisu podržani (mogu se dodati ako trebaš).

## Postavljanje kao default photo viewer

Desni klik na bilo koju .jpg → "Open with" → "Choose another app" → snapview.exe → kvačica "Always use this app".
