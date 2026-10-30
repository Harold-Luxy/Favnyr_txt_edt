# Favnyr_txt_edt

## What's new?


### 1. sort by type
<img width="450" height="438" alt="4" src="https://github.com/user-attachments/assets/c4f29617-d8af-4446-9154-5ec87622a028" />

### 2. editor window
Now you can right click a .txt/md file and open it as a window of Favnyr.
(My favourite way of editing a memo.)

<img width="2564" height="1503" alt="1" src="https://github.com/user-attachments/assets/a7d92b83-778f-4311-8689-b3d1127418cc" />
<img width="2560" height="1504" alt="2" src="https://github.com/user-attachments/assets/affbd17b-70fa-4344-8e03-6361f1dfac0e" />
<img width="2560" height="1504" alt="3" src="https://github.com/user-attachments/assets/10c6ec55-b96c-41b2-b2f0-d4b357f9c313" />

### 3. path sidebar
A tree of where the active view actually is, so a glance tells you the folder
and everything above it:

```
💾 C:\
📁 Users
📁 xinyun.lu
● 📁 Documents        ← current folder
    📁 projects        ← its subfolders
  📁 Downloads         ← its siblings
```

Click to navigate, double-click to open on the other side. It only reads
folders, never hidden ones, and network paths show the trail without probing
the share (so it cannot hang).

### 4. drag to move, Ctrl to copy
Dragging a file onto a folder now moves it straight away — no "Move / Copy /
Link" menu in the way. Hold **Ctrl** to copy instead. The little pill next to
the cursor says which one will happen before you let go.

`Ctrl+Shift` still brings the menu up, which is where **Link** lives.

### 5. undo (`Ctrl+Z`)
Takes back the last 20 operations: rename, move, copy, a newly created file or
folder, and a deletion to the Recycle Bin. Press it again to keep going back.

Two deliberate refusals:
- a permanent delete (network share, `Shift+Delete`) is never offered, because
  nothing could bring it back;
- a copy you have since edited is left alone rather than deleted, and a move
  whose old name is now taken is skipped rather than overwritten.

While the text editor has focus `Ctrl+Z` does nothing — it will not delete a
file from under a half-written note.

### 6. smaller, tidier binary
The same 39 MB single portable file, with dead code and duplicated helpers
removed. No installer, no runtime, no external DLLs.

## License

GNU General Public License v3.0 or later.
