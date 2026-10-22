# snapview

Brz, minimalan preglednik fotografija za Windows. Otvori folder pun slika, listaj strelicama, označi favorite, kopiraj odabrane u drugi folder — sve preko tipkovnice, bez nereda na ekranu.

---

## Brzi početak

1. Dvoklik na bilo koju sliku u Exploreru i odaberi **snapview** (ili postavi kao zadani preglednik — vidi dolje).
2. Ili pokreni snapview i pritisni **Ctrl+O** za odabir foldera.
3. Ili jednostavno prevuci (drag & drop) folder ili sliku u prozor.

## Tipkovni prečaci

| Tipka | Akcija |
|---|---|
| **→** / **←** | Sljedeća / prethodna slika |
| **Home** / **End** | Prva / zadnja slika |
| Tipke miša *natrag* / *naprijed* | Prethodna / sljedeća slika |
| Scroll kotačić ili **+** / **-** | Zoom in / out (scroll zumira prema kursoru) |
| **0** | Reset zoom-a |
| **Z** | HQ način (slike u punoj rezoluciji) |
| **Q** / **W** | Rotiraj lijevo / desno |
| **Space** | Označi favorit (★) |
| **F** | Filter: sve / samo favoriti / bez favorita |
| **Shift+F** | Prozor favorita (kopiranje / premještanje) |
| **I** | Prikaži EXIF info (datum, kamera, objektiv, ekspozicija) |
| **Ctrl+O** | Otvori folder |
| **Ctrl+C** | Kopiraj sliku u clipboard (puna rezolucija, okrenuta kako je prikazana) |
| **Ctrl+E** | Prikaži sliku u Exploreru |
| **F11**, **Alt+Enter** ili dvoklik | Fullscreen |
| **Delete** | Premjesti sliku u smeće |
| **Esc** | Zatvori dijalog, izađi iz fullscreena ili zatvori prozor |
| **F1** | Popis svih prečaca |
| **Desni klik** | Cijeli meni s opcijama |
| Lijevi klik + povuci | Pomakni prozor (ili sliku kad je zumirana) |
| Povuci rub prozora | Povećaj / smanji sliku (prozor uvijek završava na rubu slike) |

## Workflow s favoritima

1. Otvori folder pun slika.
2. Listaj strelicama, pritisni **Space** za one koje ti se sviđaju.
3. **Shift+F** otvara prozor sa svim označenima (**F** samo prebacuje prikaz između svih slika, favorita i ostalih).
4. Otkvači one koje na kraju ne želiš.
5. **Copy N selected to folder…** (ili **Cut…** za premještanje) odabire destinaciju — gotovo.

RAW datoteke istog imena (npr. `IMG_1234.CR3` uz `IMG_1234.JPG`) idu zajedno sa slikom. Datoteke koje već postoje u odredištu se preskaču — ništa se ne prepisuje. Kopiranje ide u pozadini, pa prozor ostaje responzivan i kod velikih serija.

Favoriti se spremaju u `.favorites.txt` u tom folderu (običan tekst, preživljava premještanje foldera).

Folder se prati: nove, obrisane i izmijenjene slike pojavljuju se same, bez ponovnog otvaranja.

## Podržani formati

JPG, PNG, BMP, GIF, WebP, TIFF.

HEIC i RAW formati trenutno nisu podržani.

## Postaviti kao zadani preglednik

Desni klik na bilo koju `.jpg` → **Open with** → **Choose another app** → **snapview** → kvačica **Always use this app**.

Thumbnaili u Exploreru ostaju isti kao prije — snapview ne mijenja ikone slika.

## Postavke i podaci

snapview pamti tvoje postavke (tema, nedavno otvarani folderi) u:

```
%LOCALAPPDATA%\snapview\
```

Deinstalacija ne briše ovaj folder — ako ga želiš ukloniti, izbriši ga ručno.

## Deinstalacija

**Settings → Apps → Installed apps → snapview → Uninstall**, ili kroz Control Panel.

---

## Pravne informacije

**snapview** — copyright © 2026 Filip Kozina. Sva prava pridržana.

### Odricanje odgovornosti

Ovaj program isporučuje se "kakav jest" (*as is*), bez ikakvog jamstva, izričitog ili podrazumijevanog, uključujući bez ograničenja jamstva utrživosti, prikladnosti za određenu svrhu i neprekršivosti prava trećih strana. Ni u kojem slučaju autor neće biti odgovoran za bilo kakvu štetu nastalu korištenjem ili nemogućnošću korištenja ovog programa, uključujući gubitak podataka, gubitak dobiti ili druge posljedične štete.

Korisnik je odgovoran za sigurnosnu kopiju vlastitih datoteka. snapview može premjestiti datoteke u smeće (Delete) — ova operacija je reverzibilna kroz Recycle Bin, ali korisnik snosi rizik vlastitih akcija.

### Third-party software

snapview je napisan u Rustu i koristi sljedeće open-source biblioteke, čije licence ostaju na snazi:

- [egui / eframe](https://github.com/emilk/egui) — MIT / Apache-2.0
- [image](https://github.com/image-rs/image) — MIT / Apache-2.0
- [rfd](https://github.com/PolyMeilex/rfd) — MIT
- [rayon](https://github.com/rayon-rs/rayon) — MIT / Apache-2.0
- [kamadak-exif](https://github.com/kamadak/exif-rs) — BSD-2-Clause
- [jpeg-decoder](https://github.com/image-rs/jpeg-decoder) — MIT / Apache-2.0
- [lcms2](https://github.com/kornelski/rust-lcms2) — MIT (wrapper) + [Little CMS](https://www.littlecms.com/) — MIT
- [notify](https://github.com/notify-rs/notify) — CC0-1.0 / Artistic-2.0
- [arboard](https://github.com/1Password/arboard) — MIT / Apache-2.0
- [trash](https://github.com/Byron/trash-rs) — MIT
- [walkdir](https://github.com/BurntSushi/walkdir) — MIT / Unlicense

Pune licence i autorska prava izvornih biblioteka dostupni su u njihovim službenim repozitorijima.

### Privatnost

snapview ne šalje nikakve podatke na internet. Sve obrade slika i metapodataka odvijaju se isključivo lokalno na tvom računalu. Aplikacija ne sadrži nikakvu telemetriju, analitiku ni automatska ažuriranja.

### Kontakt

Pitanja, bugovi ili prijedlozi: <https://github.com/Argo8/snapview/issues>
