#!/usr/bin/env python3
"""Adds a release's Linux update to the appcast generate_appcast wrote.

    add-to-appcast.py APPCAST TARBALL URL VERSION BUILD SIGNATURE

Sparkle (macOS) and LinSparkle (Linux) read the same feed, and each skips
items whose enclosure has another `sparkle:os`. A missing `sparkle:os` means
macOS to Sparkle but Linux to LinSparkle, so this marks every existing
enclosure `macos` before adding the `linux` item, which LinSparkle compares by
BUILD, the same `sparkle:version` the macOS item has.
"""

import os
import sys
import xml.etree.ElementTree as ET
from email.utils import formatdate

SPARKLE = "http://www.andymatuschak.org/xml-namespaces/sparkle"
ET.register_namespace("sparkle", SPARKLE)
ET.register_namespace("dc", "http://purl.org/dc/elements/1.1/")


def sparkle(name):
    return f"{{{SPARKLE}}}{name}"


def main():
    appcast, tarball, url, version, build, signature = sys.argv[1:]
    tree = ET.parse(appcast)
    channel = tree.getroot().find("channel")
    if channel is None:
        sys.exit(f"{appcast} has no <channel>")

    items = channel.findall("item")
    if not items:
        sys.exit(f"{appcast} has no items")
    for enclosure in channel.iter("enclosure"):
        enclosure.attrib.setdefault(sparkle("os"), "macos")

    item = ET.Element("item")
    ET.SubElement(item, "title").text = version
    ET.SubElement(item, "pubDate").text = formatdate(usegmt=True)
    link = items[-1].find("link")
    if link is None:
        link = channel.find("link")
    if link is not None and link.text:
        ET.SubElement(item, "link").text = link.text
    ET.SubElement(item, sparkle("version")).text = build
    ET.SubElement(item, sparkle("shortVersionString")).text = version
    ET.SubElement(
        item,
        "enclosure",
        {
            "url": url,
            "length": str(os.path.getsize(tarball)),
            "type": "application/gzip",
            sparkle("os"): "linux",
            sparkle("edSignature"): signature,
        },
    )
    # After the last existing item, so the feed reads newest platform last.
    channel.insert(list(channel).index(items[-1]) + 1, item)

    ET.indent(tree, space="    ")
    tree.write(appcast, encoding="utf-8", xml_declaration=True)


if __name__ == "__main__":
    main()
