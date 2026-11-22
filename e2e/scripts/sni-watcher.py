#!/usr/bin/env python3
"""A minimal org.kde.StatusNotifierWatcher for the headless Linux E2E run.

A desktop panel (GNOME's AppIndicator extension, KDE, waybar…) runs this watcher; Xvfb has none, so
an app's tray icon would have nowhere to register. This one accepts the registration and records the
item's bus name + object path in /tmp/sni-items.txt, so the test can read the tray menu over
com.canonical.dbusmenu and click its items, exactly as a panel would.
"""
from gi.repository import Gio, GLib

XML = """
<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
    <method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
    <signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
    <signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
    <signal name="StatusNotifierHostRegistered"/>
  </interface>
</node>
"""
items = []


def on_call(conn, sender, path, iface, method, params, inv):
    if method == "RegisterStatusNotifierItem":
        arg = params.unpack()[0]
        item = sender + arg if arg.startswith("/") else arg + "/StatusNotifierItem"
        items.append(item)
        with open("/tmp/sni-items.txt", "a") as f:
            f.write(item + "\n")
        conn.emit_signal(None, path, iface, "StatusNotifierItemRegistered", GLib.Variant("(s)", (item,)))
    inv.return_value(None)


def on_get(conn, sender, path, iface, prop):
    if prop == "RegisteredStatusNotifierItems":
        return GLib.Variant("as", items)
    if prop == "IsStatusNotifierHostRegistered":
        return GLib.Variant("b", True)
    return GLib.Variant("i", 0)


def on_bus(conn, name):
    node = Gio.DBusNodeInfo.new_for_xml(XML)
    conn.register_object("/StatusNotifierWatcher", node.interfaces[0], on_call, on_get, None)


Gio.bus_own_name(Gio.BusType.SESSION, "org.kde.StatusNotifierWatcher", Gio.BusNameOwnerFlags.NONE, on_bus, None, None)
open("/tmp/sni-items.txt", "w").close()
GLib.MainLoop().run()
