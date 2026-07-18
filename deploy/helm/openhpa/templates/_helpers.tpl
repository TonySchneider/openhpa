{{- define "openhpa.name" -}}openhpa{{- end -}}

{{- define "openhpa.serviceAccountName" -}}{{ .Values.serviceAccount.name }}{{- end -}}

{{- define "openhpa.labels" -}}
app.kubernetes.io/name: {{ include "openhpa.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end -}}

{{- define "openhpa.selectorLabels" -}}
app.kubernetes.io/name: {{ include "openhpa.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
