!define SHIORI_PROGID "Shiori.Markdown"
!define SHIORI_APP "Software\Classes\Applications\${MAINBINARYNAME}.exe"
!define SHIORI_CAPABILITIES "Software\ShioriMDE\Capabilities"

!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr SHCTX "Software\Classes\${SHIORI_PROGID}" "" "Markdown 文書"
  WriteRegStr SHCTX "Software\Classes\${SHIORI_PROGID}\DefaultIcon" "" '$\"$INSTDIR\${MAINBINARYNAME}.exe$\",0'
  WriteRegStr SHCTX "Software\Classes\${SHIORI_PROGID}\shell\open\command" "" '$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\"'
  WriteRegStr SHCTX "Software\Classes\.md\OpenWithProgids" "${SHIORI_PROGID}" ""
  WriteRegStr SHCTX "${SHIORI_APP}" "FriendlyAppName" "${PRODUCTNAME}"
  WriteRegStr SHCTX "${SHIORI_APP}\SupportedTypes" ".md" ""
  WriteRegStr SHCTX "${SHIORI_APP}\shell\open\command" "" '$\"$INSTDIR\${MAINBINARYNAME}.exe$\" $\"%1$\"'
  WriteRegStr SHCTX "${SHIORI_CAPABILITIES}" "ApplicationName" "${PRODUCTNAME}"
  WriteRegStr SHCTX "${SHIORI_CAPABILITIES}" "ApplicationDescription" "Markdown Editor"
  WriteRegStr SHCTX "${SHIORI_CAPABILITIES}\FileAssociations" ".md" "${SHIORI_PROGID}"
  WriteRegStr SHCTX "Software\RegisteredApplications" "${PRODUCTNAME}" "${SHIORI_CAPABILITIES}"
  !insertmacro UPDATEFILEASSOC
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue SHCTX "Software\RegisteredApplications" "${PRODUCTNAME}"
  DeleteRegKey SHCTX "${SHIORI_CAPABILITIES}"
  DeleteRegKey /ifempty SHCTX "Software\ShioriMDE"
  DeleteRegValue SHCTX "Software\Classes\.md\OpenWithProgids" "${SHIORI_PROGID}"
  DeleteRegKey SHCTX "Software\Classes\${SHIORI_PROGID}"
  DeleteRegKey SHCTX "${SHIORI_APP}"
  !insertmacro UPDATEFILEASSOC
!macroend
