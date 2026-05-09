; Windows Explorer shell integration for RetinaTag.
; Adds:
;   1. A "Tag with RetinaTag" entry to the right-click context menu for folders
;      and image files. Clicking it launches RetinaTag with the path passed
;      as a --scan-path argument.
;   2. A "RetinaTag" association for images so users can pick the app in
;      "Open with…".
;
; Clean uninstall: every key this file writes is deleted in the uninstall
; hook so upgrading / uninstalling never leaves orphan registry entries.

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Registering shell integration…"

  ; ── Folder context menu ──
  WriteRegStr HKCU "Software\Classes\Directory\shell\RetinaTag" "" "Tag with RetinaTag"
  WriteRegStr HKCU "Software\Classes\Directory\shell\RetinaTag" "Icon" "$INSTDIR\retina-tag.exe,0"
  WriteRegStr HKCU "Software\Classes\Directory\shell\RetinaTag\command" "" '"$INSTDIR\retina-tag.exe" "--scan-path" "%1"'

  ; Also expose it on the folder background (right-click inside a folder)
  WriteRegStr HKCU "Software\Classes\Directory\Background\shell\RetinaTag" "" "Tag with RetinaTag"
  WriteRegStr HKCU "Software\Classes\Directory\Background\shell\RetinaTag" "Icon" "$INSTDIR\retina-tag.exe,0"
  WriteRegStr HKCU "Software\Classes\Directory\Background\shell\RetinaTag\command" "" '"$INSTDIR\retina-tag.exe" "--scan-path" "%V"'

  ; ── Image file context menu (single file "Open with RetinaTag") ──
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\image\shell\RetinaTag" "" "Open with RetinaTag"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\image\shell\RetinaTag" "Icon" "$INSTDIR\retina-tag.exe,0"
  WriteRegStr HKCU "Software\Classes\SystemFileAssociations\image\shell\RetinaTag\command" "" '"$INSTDIR\retina-tag.exe" "--scan-path" "%1"'

  ; Capabilities key for "Open with…"
  WriteRegStr HKCU "Software\RetinaTag\Capabilities" "ApplicationName" "RetinaTag"
  WriteRegStr HKCU "Software\RetinaTag\Capabilities" "ApplicationDescription" "AI-powered photo library tagger"
  WriteRegStr HKCU "Software\RegisteredApplications" "RetinaTag" "Software\RetinaTag\Capabilities"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Removing shell integration…"

  DeleteRegKey HKCU "Software\Classes\Directory\shell\RetinaTag"
  DeleteRegKey HKCU "Software\Classes\Directory\Background\shell\RetinaTag"
  DeleteRegKey HKCU "Software\Classes\SystemFileAssociations\image\shell\RetinaTag"
  DeleteRegKey HKCU "Software\RetinaTag"
  DeleteRegValue HKCU "Software\RegisteredApplications" "RetinaTag"
!macroend
