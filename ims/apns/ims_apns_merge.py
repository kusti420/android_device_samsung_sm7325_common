#!/usr/bin/env python3
#
# Copyright (C) 2026 IchthysOS
# SPDX-License-Identifier: Apache-2.0
#
"""Applies ims_apns_overrides.xml to the AOSP APN list (device/sample/etc/apns-full-conf.xml).

The overrides are generated at dev time by ims/tools/gen_carrier_ims.py from the stock firmware's
IMS APNs. Two kinds of entries:

  <fix mcc= mnc= apn= attr= from= to= />   change one attribute of the AOSP IMS APN of that
                                           operator (from="" means the attribute is absent)
  <add mcc= mnc= apn= ... />               add an IMS APN the AOSP list does not have

The AOSP list is copied text-for-text otherwise. Any override that no longer matches the AOSP list
(entry gone, changed, duplicated, or AOSP gained the APN we add) fails the build, so a changed
upstream list is noticed instead of being patched twice or not at all.

usage: ims_apns_merge.py <apns-full-conf.xml> <ims_apns_overrides.xml>  (result on stdout)
"""

import re
import sys
import xml.etree.ElementTree as ET

APN_RE = re.compile(r'<apn\b.*?/>', re.S)
ATTR_RE = re.compile(r'([A-Za-z_]+)\s*=\s*"([^"]*)"')


def attrs(block):
    return dict(ATTR_RE.findall(block))


def is_ims(a):
    return 'ims' in [t.strip() for t in a.get('type', '').split(',')]


def ims_apns(text):
    """(match, attrs) of every non-MVNO APN that carries the ims type."""
    out = []
    for m in APN_RE.finditer(text):
        a = attrs(m.group(0))
        if is_ims(a) and not a.get('mvno_type'):
            out.append((m, a))
    return out


def find(entries, mcc, mnc, apn=None):
    return [(m, a) for m, a in entries
            if a.get('mcc') == mcc and a.get('mnc') == mnc
            and (apn is None or a.get('apn', '').lower() == apn.lower())]


def set_attr(block, name, value):
    pat = re.compile(r'(\b%s\s*=\s*")[^"]*(")' % re.escape(name))
    if pat.search(block):
        return pat.sub(lambda m: m.group(1) + value + m.group(2), block, count=1)
    # Add the attribute before the closing "/>", on its own line like the others.
    i = block.rfind('/>')
    return block[:i].rstrip() + '\n      %s="%s"\n  ' % (name, value) + block[i:]


ADD_ORDER = ['carrier', 'carrier_id', 'mcc', 'mnc', 'apn', 'type', 'protocol',
             'roaming_protocol', 'authtype', 'user_visible']


def render_add(a):
    lines = []
    for k in ADD_ORDER + sorted(k for k in a if k not in ADD_ORDER):
        if k in a:
            sep = ' = ' if k == 'carrier_id' else '='
            lines.append('%s%s"%s"' % (k, sep, a[k]))
    return '  <apn ' + '\n      '.join(lines) + '\n  />\n'


def merge(text, overrides):
    errors = []
    entries = ims_apns(text)
    edits = []  # (start, end, replacement)
    inserts = {}  # offset -> [text]

    for o in overrides:
        a = dict(o.attrib)
        if o.tag == 'fix':
            hits = find(entries, a['mcc'], a['mnc'], a['apn'])
            if len(hits) != 1:
                errors.append('fix %s%s %s: %d AOSP IMS APNs match' %
                              (a['mcc'], a['mnc'], a['apn'], len(hits)))
                continue
            m, cur = hits[0]
            if cur.get(a['attr'], '') != a['from']:
                errors.append('fix %s%s %s: %s is "%s", expected "%s"' %
                              (a['mcc'], a['mnc'], a['apn'], a['attr'],
                               cur.get(a['attr'], ''), a['from']))
                continue
            edits.append((m.start(), m.end(), a['attr'], a['to']))
        elif o.tag == 'add':
            if find(entries, a['mcc'], a['mnc']):
                errors.append('add %s%s: AOSP now has an IMS APN, drop the addition'
                              % (a['mcc'], a['mnc']))
                continue
            # After the operator's last AOSP APN, else before </apns>.
            pos = None
            for m in APN_RE.finditer(text):
                b = attrs(m.group(0))
                if b.get('mcc') == a['mcc'] and b.get('mnc') == a['mnc']:
                    pos = m.end()
            if pos is None:
                pos = text.rindex('</apns>')
                inserts.setdefault(pos, []).append(render_add(a) + '\n')
            else:
                inserts.setdefault(pos, []).append('\n\n' + render_add(a).rstrip('\n'))
        else:
            errors.append('unknown override <%s>' % o.tag)

    if errors:
        for e in errors:
            print('ims_apns_merge: ' + e, file=sys.stderr)
        sys.exit(1)

    # Apply edits per block (several fixes may touch the same block).
    blocks = {}
    for start, end, name, value in edits:
        blocks.setdefault((start, end), []).append((name, value))
    points = sorted(set([k[0] for k in blocks] + list(inserts)))
    out, last = [], 0
    for p in points:
        out.append(text[last:p])
        last = p
        for (start, end), changes in blocks.items():
            if start == p:
                b = text[start:end]
                for name, value in changes:
                    b = set_attr(b, name, value)
                out.append(b)
                last = end
        if p in inserts:
            out.extend(inserts[p])
    out.append(text[last:])
    return ''.join(out)


def main(argv):
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    with open(argv[1], encoding='utf-8') as f:
        text = f.read()
    overrides = list(ET.parse(argv[2]).getroot())
    sys.stdout.write(merge(text, overrides))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
