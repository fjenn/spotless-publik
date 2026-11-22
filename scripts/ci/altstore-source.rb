#!/usr/bin/env ruby
# frozen_string_literal: true
# Write an AltStore/SideStore source (https://faq.altstore.io/developers/make-a-source) for one
# unsigned Spotless IPA. Used by .github/workflows/release-ios.yml, which attaches the result to the
# ios-v* GitHub release as altstore.json. The public source at https://opena.tech/spotless/altstore.json
# (the opena site) reads this file from the newest iOS release for the facts only the IPA knows
# (permissions, minimum iOS), so the source can never disagree with the IPA it points at: AltStore
# refuses to install an app whose entitlements or usage descriptions differ from the source's.
#
#   ruby scripts/ci/altstore-source.rb <Spotless.app> <ipa file> <download url> > altstore.json
#
# The IPA is unsigned (SideStore/AltStore sign it with the user's Apple ID), so it carries no
# entitlements: appPermissions.entitlements is [] and appPermissions.privacy is every
# NS*UsageDescription in the app's Info.plist. Ruby stdlib + plutil only (stock macOS runner).
require "json"
require "time"

app, ipa, url = ARGV
abort "usage: altstore-source.rb <Spotless.app> <ipa> <download url>" unless app && ipa && url

info = JSON.parse(IO.popen(["plutil", "-convert", "json", "-o", "-", File.join(app, "Info.plist")], &:read))
version = info.fetch("CFBundleShortVersionString")
build = info.fetch("CFBundleVersion")
min_os = info["MinimumOSVersion"] || "17.0"
privacy = info.select { |k, _| k.start_with?("NS") && k.end_with?("UsageDescription") }.sort.to_h
size = File.size(ipa)
date = Time.now.utc.iso8601
site = "https://opena.tech"

out = {
  name: "Spotless",
  identifier: "app.spotlessmusic.source",
  subtitle: "Free music, podcasts and audiobooks. No ads.",
  description: "The official source for Spotless for iPhone. Free and open source (GPL-3.0).",
  iconURL: "#{site}/icons/spotless.webp",
  website: "#{site}/spotless",
  tintColor: "#FF2D7A",
  apps: [{
    name: "Spotless",
    bundleIdentifier: "app.spotlessmusic.ios",
    developerName: "opena",
    subtitle: "Free music, no ads.",
    localizedDescription: "Songs, albums, podcasts and free audiobooks with synced lyrics, offline downloads and playlist import. No account, no ads, no tracking.",
    iconURL: "#{site}/icons/spotless.webp",
    tintColor: "#FF2D7A",
    category: "entertainment",
    screenshots: %w[home now-playing lyrics search library podcast audiobooks import].map do |s|
      { imageURL: "#{site}/media/spotless/#{s}.webp", width: 600, height: 1304 }
    end,
    versions: [{
      version: version,
      buildVersion: build,
      date: date,
      localizedDescription: "Spotless #{version} (#{build}).",
      downloadURL: url,
      size: size,
      minOSVersion: min_os,
    }],
    appPermissions: { entitlements: [], privacy: privacy },
  }],
  news: [],
}
puts JSON.pretty_generate(out)
