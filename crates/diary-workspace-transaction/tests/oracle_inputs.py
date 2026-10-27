"""Synthetic input specifications, not expected Rust or Python results."""

TENANT = "01234567-89ab-4cde-8fab-0123456789ab"
NAMESPACE = "11234567-89ab-4cde-8fab-0123456789ab"
OP = "21234567-89ab-4cde-8fab-0123456789ab"
CAP = "31234567-89ab-4cde-8fab-0123456789ab"
NOW = 1700000000.0

READ_FILES = [
    ("Notes/one.md", b"synthetic note", 10.0),
    ("Notes/Zebra.md", b"z", 20.0),
    ("Notes/apple.MD", b"a", 30.0),
    ("Notes/a.md", b"lower tie first", 31.0),
    ("Notes/A.md", b"upper tie second", 32.0),
    ("Notes/ΣΟΣ.md", b"unicode", 33.0),
    ("Notes/ignore.txt", b"not markdown", 40.0),
    ("Notes/.hidden.md", b"hidden", 50.0),
    ("Other/sentinel.md", b"unrelated", 60.0),
]
READ_DIRS = ["Notes", "Notes/Empty", "Empty"]
READ_CASES = [("stat", "Notes/one.md"), ("stat", "Notes"), ("stat", "Empty"),
              ("stat", "Missing"), ("stat", "/Notes/one.md/"),
              ("list", "Notes"), ("list", ""), ("list", "Empty")]
UNICODE_CASES = [
    ("new-ramshorn", ["\ua7cba.md", "\u0264z.md"]),
    ("new-cyrillic", ["\u1c89z.md", "\u1c8aa.md"]),
    ("sigma-followed-new", ["AΣ\u1c89a.md", "Aς\u1c89z.md"]),
    ("sigma-after-new", ["\u1c89Σ1.md", "\u1c89σ0.md"]),
    ("dotted-i", ["İz.md", "I\u0307a.md"]),
    ("ordinary", ["Zebra.md", "apple.MD", "ΣΟΣ.md"]),
]

FILES=[('Notes/one.md',b'synthetic note'),('Notes/existing.md',b'existing bytes'),('Notes/Folder/a.md',b'a'),('Notes/Folder/b.md',b'b'),('Entries/capture.md',b'protected capture'),('AI Memory/standing.md',b'protected memory'),('Monthly/index.md',b'protected index'),('Monthly/2026-10.md',b'protected month'),('Other/sentinel.md',b'unrelated')]

DIRS=['Notes','Notes/Empty','Notes/Folder','Notes/Folder/Empty','Dest']

CASES=[
 {'name':'copy-file','op':'copy','path':'Notes/one.md','destination':'Dest/copied.md'},
 {'name':'copy-explicit-null','op':'copy','path':'Notes/one.md','destination':'Dest/copied.md','version':None},
 {'name':'copy-wrong-version','op':'copy','path':'Notes/one.md','destination':'Dest/copied.md','version':'wrong'},
 {'name':'move-file','op':'move','path':'Notes/one.md','destination':'Dest/moved.md','version':'$source'},
 {'name':'move-folder','op':'move','path':'Notes/Folder','destination':'Dest/Moved','version':'$source'},
 {'name':'copy-folder','op':'copy','path':'Notes/Folder','destination':'Dest/Copied'},
 {'name':'copy-empty-directory','op':'copy','path':'Notes/Empty','destination':'Dest/Empty'},
 {'name':'delete-file','op':'delete','path':'Notes/one.md','version':'$source'},
 {'name':'delete-folder','op':'delete','path':'Notes/Folder','version':'$source'},
 {'name':'delete-empty-directory','op':'delete','path':'Notes/Empty','version':'$source'},
 {'name':'preserve-file','op':'preserve','path':'Notes/one.md','version':'$source'},
 {'name':'preserve-protected','op':'preserve','path':'Entries/capture.md','version':'$source'},
 {'name':'delete-protected','op':'delete','path':'Entries/capture.md','version':'$source'},
 {'name':'copy-to-protected','op':'copy','path':'Notes/one.md','destination':'AI Memory/new.md'},
 {'name':'move-to-protected','op':'move','path':'Notes/one.md','destination':'AI Memory/new.md','version':'$source'},
 {'name':'copy-self','op':'copy','path':'Notes/one.md','destination':'Notes/one.md'},
 {'name':'copy-non-markdown-destination','op':'copy','path':'Notes/one.md','destination':'Dest/file.txt'},
 {'name':'move-missing-parent','op':'move','path':'Notes/one.md','destination':'Missing/file.md','version':'$source'},
 {'name':'copy-existing-without-overwrite','op':'copy','path':'Notes/one.md','destination':'Notes/existing.md'},
 {'name':'copy-overwrite-file','op':'copy','path':'Notes/one.md','destination':'Notes/existing.md','overwrite':True,'destinationVersion':'$destination'},
 {'name':'copy-overwrite-wrong-destination','op':'copy','path':'Notes/one.md','destination':'Notes/existing.md','overwrite':True,'destinationVersion':'wrong'},
 {'name':'copy-overwrite-folder','op':'copy','path':'Notes/Folder','destination':'Notes/Empty','overwrite':True,'destinationVersion':'$destination'},
 {'name':'delete-missing','op':'delete','path':'Notes/missing.md','version':'wrong'},
 {'name':'delete-folder-with-hidden-type','op':'delete','path':'Notes/Folder','version':'$source','profile':'nonmarkdown'},
 {'name':'delete-folder-bound','op':'delete','path':'Wide','version':'$source','profile':'wide'},
 {'name':'copy-large-generation','op':'copy','path':'Notes/one.md','destination':'Dest/copied.md','generation':'9'*100},
 {'name':'trash-file','op':'trash','path':'Notes/one.md','version':'$source'},
 {'name':'trash-already-applied','op':'trash','path':'Notes/one.md','version':'$original','prepare':'trashed'},
 {'name':'trash-id-version-mismatch','op':'trash','path':'Notes/one.md','version':'wrong','prepare':'trashed'},
 {'name':'trash-protected','op':'trash','path':'Entries/capture.md','version':'$source'},
 {'name':'restore-file','op':'restore','prepare':'trashed'},
 {'name':'restore-already-restored','op':'restore','prepare':'restored'},
 {'name':'restore-occupied','op':'restore','prepare':'occupied'},
 {'name':'trash-list','op':'trashList','prepare':'trashed'},
 {'name':'preserve-oversized-capsule','op':'preserve','path':'Notes/one.md','version':'$source','profile':'large'},
]

EXTRA_CASES=[
 {'name':'copy-missing-to-protected','op':'copy','path':'Notes/missing.md','destination':'AI Memory/new.md'},
 {'name':'delete-missing-protected','op':'delete','path':'Entries/missing.md','version':'wrong'},
 {'name':'copy-wide-wrong-version','op':'copy','path':'Wide','destination':'Dest/Wide','version':'wrong','profile':'wide'},
 {'name':'delete-wide-without-version','op':'delete','path':'Wide','profile':'wide'},
 {'name':'delete-wide-wrong-version','op':'delete','path':'Wide','version':'wrong','profile':'wide'},
 {'name':'copy-null-with-nonmarkdown-destination','op':'copy','path':'Notes/one.md','destination':'Dest/file.txt','version':None},
 {'name':'move-wrong-version-missing-parent','op':'move','path':'Notes/one.md','destination':'Missing/file.md','version':'wrong'},
 {'name':'preserve-missing-protected','op':'preserve','path':'Entries/missing.md','version':'wrong'},
 {'name':'move-protected-wrong-version','op':'move','path':'Entries/capture.md','destination':'Dest/capture.md','version':'wrong'},
 {'name':'copy-folder-nonmarkdown-child','op':'copy','path':'Notes/Folder','destination':'Dest/Folder','profile':'nonmarkdown'},
]
