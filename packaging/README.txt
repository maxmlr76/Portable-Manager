MAXTRIX PORTABLE MANAGER
========================

A portable manager for your portable tools. Runs directly from a USB drive,
no installation needed.

IN THIS FOLDER
--------------
  PortableManager.exe     The app. Double-click to start.
  vendor\7-zip\           Bundled 7-Zip (unpacks installers / .7z after download).
  Tools\                  Your tools (one subfolder per category).
  README.txt              This file. LIESMICH.txt is the German version.
  THIRD-PARTY-NOTICES.txt Notices for bundled software (7-Zip, Slint, fonts).
  licenses\               License texts. Maxtrix itself is MIT, name and logo excluded.

PUTTING IT ON A DRIVE
---------------------
  Copy the ENTIRE content of this folder to the root of your USB drive, for
  example to E:\  so that E:\PortableManager.exe, E:\vendor\ and E:\Tools\
  exist there. Then double-click PortableManager.exe.

  The drive letter does not matter: the app keeps paths relative to the drive.

USAGE
-----
  - EN / DE at the bottom left switches the language.
  - "Discover": install curated tools. Click a card to see the source, checksum,
    dependencies and size before downloading.
  - "Installed": everything on the drive. Launch, open folder, remove. Tools you
    copy to Tools\<Category>\<Name>\ yourself also show up here.
  - "Updates": shows when a newer version is available.
  - "My lists" (left): your own collections. Add tools with the list icon at the
    top right of a card, or drag that icon onto a list.
  - Share a list: open it and click "Export...". On another stick, import the
    file with the arrow icon next to "My lists", then "Install missing".
    Tools come only from the built-in catalog, never from the file.
  - App updates: when a newer Portable Manager is released, a card in the
    sidebar offers it. The download is checked before anything is replaced;
    your Tools folder and settings stay as they are. Turn the check off in the
    tray menu ("Check for app updates").
  - Command-line tools (e.g. ripgrep, GnuPG) have "Terminal" instead of "Launch":
    a console opens in the tool folder with the tool on PATH.
  - Keyboard: Ctrl+F = search, Esc = close dialog or details.

NOTES
-----
  - Downloads come from the official publishers and are verified by checksum.
  - If the EXE is not signed with a trusted certificate, Windows SmartScreen may
    warn at first start ("More info" -> "Run anyway").
  - Some tools need extra drivers (e.g. Nmap: Npcap). No portable launcher can
    replace those.
  - Tray icon: left click opens or closes the quick start, right click shows the menu.
  - At the first close the app asks whether to keep running in the notification
    area or to quit. Change it later by right-clicking the tray icon.
  - If the window stays white or blank on a PC (graphics driver): right-click the
    tray icon, tick "Simple graphics", quit and restart the app.
  - BEFORE UPDATING the app, quit it from the tray icon. Otherwise the old EXE is
    locked and is not replaced when copying.
  - Packages that install only through a setup program are left out on purpose:
    they would set up on the PC instead of the drive.
  - Tool settings and data stay in the tool's folder on the drive and survive updates.
  - "Cancel" stops installs and updates. A cancelled or failed update changes
    nothing: the previous version still starts.
  - A running tool cannot be updated. Close it first.

LIABILITY
---------
  The software comes without warranty (see licenses\LICENSE.txt). Use security tools
  such as Nmap or hashcat only on your own systems or with permission.

Version 0.1.1
