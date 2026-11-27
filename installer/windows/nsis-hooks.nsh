; Vortex Download Manager — Per-User NSIS Native Messaging Hooks (P1-T37)
; Registers com.vortex.bridge in HKCU (zero Admin UAC!) for Brave, Chrome, and Edge.

!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr HKCU "Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.vortex.bridge" "" "$INSTDIR\com.vortex.bridge.json"
  WriteRegStr HKCU "Software\Google\Chrome\NativeMessagingHosts\com.vortex.bridge" "" "$INSTDIR\com.vortex.bridge.json"
  WriteRegStr HKCU "Software\Microsoft\Edge\NativeMessagingHosts\com.vortex.bridge" "" "$INSTDIR\com.vortex.bridge.json"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DeleteRegKey HKCU "Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.vortex.bridge"
  DeleteRegKey HKCU "Software\Google\Chrome\NativeMessagingHosts\com.vortex.bridge"
  DeleteRegKey HKCU "Software\Microsoft\Edge\NativeMessagingHosts\com.vortex.bridge"
!macroend
