#!/bin/sh
# DockNeighbor hub-lite — /cgi-bin/gps, the LAN GPS read (0.17.0, telemetry design §A7.11a). Installed at
# /www/brvg/cgi-bin/gps beside the webhook receiver and served by the same uhttpd on 8722.
#
# The design names this path, so it exists; the ONE implementation is the /api/hub door's
# GET /gps/live (hub-lite-api.sh), keyed like /api/hub/status with the caller's own member key as
# `Authorization: Bearer`. This file only routes there, so the auth, the answer and the owner's D1 line
# (a JSON read endpoint, never a local web page) live in one place.
# Exported, not prefixed: assignments before the special builtin `exec` need not reach the new program.
REQUEST_METHOD=GET
PATH_INFO=/gps/live
export REQUEST_METHOD PATH_INFO
exec sh "${BRVG_HUB_LITE_API:-/www/brvg/api/hub}"
