module github.com/nicholasgasior/wail/wail-app

go 1.26.1

require github.com/gorilla/websocket v1.5.3

require (
	github.com/DatanoiseTV/abletonlink-go v0.0.0-20260221181029-2b72c552081d
	github.com/google/uuid v1.6.0
)

replace github.com/DatanoiseTV/abletonlink-go => /tmp/abletonlink-go-build
