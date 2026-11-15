#!/usr/bin/env python3
"""Adds one platform's update to the appcast generate_appcast wrote.

    add-to-appcast.py APPCAST FILE URL VERSION BUILD SIGNATURE --os OS \
        --type MIME [--installer-arguments ARGS]

Sparkle (macOS), LinSparkle (Linux) and WinSparkle (Windows) read the same
feed, and each skips items whose enclosure has another `sparkle:os`. A
missing `sparkle:os` means macOS to Sparkle but any platform to LinSparkle
and WinSparkle, so this marks every enclosure without one `macos` before
adding the new item, which each updater compares by BUILD, the same
`sparkle:version` the macOS item has.

OS is `linux`, or `windows-x64` or `windows-arm64`: WinSparkle only takes
items for its own architecture. Each call adds one item, after the last.
"""

import argparse
import os
import xml.etree.ElementTree as ET
from email.utils import formatdate

SPARKLE = "http://www.andymatuschak.org/xml-namespaces/sparkle"
ET.register_namespace("sparkle", SPARKLE)
ET.register_namespace("dc", "http://purl.org/dc/elements/1.1/")


def sparkle(name):
    return f"{{{SPARKLE}}}{name}"


def main():
    parser = argparse.ArgumentParser(description="Add an update to an appcast.")
    for name in ("appcast", "file", "url", "version", "build", "signature"):
        parser.add_argument(name)
    parser.add_argument("--os", required=True)
    parser.add_argument("--type", required=True, help="the enclosure's MIME type")
    parser.add_argument(
        "--installer-arguments",
        help="WinSparkle passes these to the installer it runs",
    )
    args = parser.parse_args()

    tree = ET.parse(args.appcast)
    channel = tree.getroot().find("channel")
    if channel is None:
        parser.exit(1, f"{args.appcast} has no <channel>\n")

    items = channel.findall("item")
    if not items:
        parser.exit(1, f"{args.appcast} has no items\n")
    for enclosure in channel.iter("enclosure"):
        enclosure.attrib.setdefault(sparkle("os"), "macos")

    item = ET.Element("item")
    ET.SubElement(item, "title").text = args.version
    ET.SubElement(item, "pubDate").text = formatdate(usegmt=True)
    link = items[-1].find("link")
    if link is None:
        link = channel.find("link")
    if link is not None and link.text:
        ET.SubElement(item, "link").text = link.text
    ET.SubElement(item, sparkle("version")).text = args.build
    ET.SubElement(item, sparkle("shortVersionString")).text = args.version
    attributes = {
        "url": args.url,
        "length": str(os.path.getsize(args.file)),
        "type": args.type,
        sparkle("os"): args.os,
        sparkle("edSignature"): args.signature,
    }
    if args.installer_arguments:
        attributes[sparkle("installerArguments")] = args.installer_arguments
    ET.SubElement(item, "enclosure", attributes)
    # After the last existing item, so the feed reads newest platform last.
    channel.insert(list(channel).index(items[-1]) + 1, item)

    ET.indent(tree, space="    ")
    tree.write(args.appcast, encoding="utf-8", xml_declaration=True)


if __name__ == "__main__":
    main()
