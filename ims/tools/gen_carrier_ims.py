#!/usr/bin/env python3
#
# Copyright (C) 2026 IchthysOS
# SPDX-License-Identifier: Apache-2.0
#
"""Generates the IchthysOS VoLTE carrier data from the stock Samsung IMS configuration.

Run at dev time; the outputs are checked in and the build only consumes them:

  ims/overlays/carrierconfig/res/xml/vendor.xml   CarrierConfig bundles (global + per operator)
  ims/apns/ims_apns_overrides.xml                IMS APN fixes/additions for the product apns-conf
  ims/tools/carrier_ims_coverage.tsv             what was generated for which PLMN, and why

Inputs (nothing from them is copied verbatim; only operator facts are extracted):
  --imsservice  the stock imsservice.apk (system/priv-app/imsservice/) of an SM-A528B firmware.
                Its res/raw/{mnomap,imsswitch,imsprofile,globalsettings}.json say, per operator,
                whether Samsung turns VoLTE on and with which IMS parameters.
  --stock-apns  the same firmware's system/etc/apns-conf.xml (IMS APN names and IP types).
  AOSP, from this tree: device/sample/etc/apns-full-conf.xml and TelephonyProvider's
                carrier_list.textpb (carrier ids, operator names).

Policy that is ours, not Samsung's (GLOBAL, VERIFIED, NO_CS below) is documented where it is
defined and in VOLTE_A52SXQ_PLAN.md.

usage:
  gen_carrier_ims.py --imsservice <imsservice.apk> --stock-apns <apns-conf.xml> [--check]
"""

import argparse
import collections
import hashlib
import io
import json
import os
import re
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
IMS = os.path.dirname(HERE)
TREE = os.path.normpath(os.path.join(IMS, '..', '..', '..', '..'))
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(IMS, 'apns'))
import ims_apns_merge as apnmerge  # noqa: E402

AOSP_APNS = os.path.join(TREE, 'device/sample/etc/apns-full-conf.xml')
CARRIER_LIST = os.path.join(
    TREE, 'packages/providers/TelephonyProvider/assets/latest_carrier_id/carrier_list.textpb')
OUT_VENDOR = os.path.join(IMS, 'overlays/carrierconfig/res/xml/vendor.xml')
OUT_APNS = os.path.join(IMS, 'apns/ims_apns_overrides.xml')
OUT_TSV = os.path.join(HERE, 'carrier_ims_coverage.tsv')

# AccessNetworkType
EUTRAN, NGRAN = 3, 6

# ---------------------------------------------------------------------------------------------
# Policy
# ---------------------------------------------------------------------------------------------

# Default UA when an operator has no translatable format of its own: Samsung's default profile
# format "[PRODUCT_MODEL]-[BUILD_VERSION] Samsung IMS 6.0". [BUILD_VERSION] is the firmware (PDA)
# version; on IchthysOS ro.bootloader still names the user's last stock firmware (ImsStack
# #BOOTLOADER# token, packages/modules/ImsStack ich/volte-imsstack).
DEFAULT_UA = '#MODEL#-#BOOTLOADER# Samsung IMS 6.0'

# Applies to every SIM (first bundle, no filter); per-operator bundles below override it.
GLOBAL = [
    ('bool', 'carrier_volte_available_bool', False,
     'VoLTE only where the stock firmware turns it on (per-operator bundles below).'),
    ('bool', 'carrier_vt_available_bool', False, 'No IMS video path on this device.'),
    ('bool', 'carrier_wfc_ims_available_bool', False, 'No Iwlan/ePDG shipped: no VoWiFi.'),
    ('bool', 'carrier_use_ims_first_for_emergency_bool', False,
     'Emergency calls go CS-first wherever the network has CS (GsmCdmaPhone dials them as before\n'
     '             ImsStack). Only networks without any CS domain (NO_CS) dial IMS first.'),
    ('bool', 'imssms.sms_over_ims_supported_bool', False,
     'SMS stays on CS/SGs (keeps e.g. the Mobile-ID STK SEND_SMS path unchanged); only\n'
     '             networks without CS (NO_CS) send SMS over IMS.'),
    ('bool', 'carrier_supports_ss_over_ut_bool', False, 'Supplementary services stay on CS.'),
    ('bool', 'ims.enable_presence_publish_bool', False, 'No RCS/presence.'),
    ('bool', 'use_rcs_sip_options_bool', False, None),
    ('bool', 'imsvoice.carrier_volte_roaming_available_bool', False,
     'No VoLTE while roaming: ImsStack never puts roaming voice on the default bearer (IR.92\n'
     '             2.4.3.1) and IRadio@1.5 reports no dedicated-bearer QoS, so roaming calls would\n'
     '             be released after the bearer wait. Roaming calls use CS/CSFB.'),
    ('bool', 'imsvoice.voice_on_default_bearer_supported_bool', True,
     'IRadio@1.5 carries no QoS sessions, so ImsStack never sees the QCI-1 bearer the network\n'
     '             sets up (the modem still routes RTP onto it by TFT). Without this the 8 s\n'
     '             dedicated-bearer wait expires and ImsStack releases every call.'),
    ('bool', 'imsvoice.audio_evs_support_bool', False,
     'ImsMedia has no EVS codec on this device: offer AMR-WB/AMR only.'),
    ('string', 'ims.ims_user_agent_string', DEFAULT_UA,
     'Stock Samsung IMS User-Agent format. Telia EE answered ImsStack\'s default\n'
     '             "samsung_SM-A528B_Android17_<build id>" with a bare 403 and challenged (401) this\n'
     '             one (verified 2026-11-08 on 248-01).'),
    ('int', 'ims.max_allowed_network_mtu_int', 1440,
     'TCP MSS 1300 on IPv6 (1440 - ESP/TCP/IPv6/20 B overhead), as stock\'s usual mss_size=1300:\n'
     '             with the PDN\'s 1500 the IPsec-protected REGISTER (2.2 kB) never arrived and the\n'
     '             P-CSCF reset the connection (verified on 248-01). Operators whose stock mss_size\n'
     '             is smaller get a smaller MTU below.'),
    ('int-array', 'ims.registration_permanent_error_code_int_array', [403],
     'A REGISTER answered 403 is not retried until reboot/airplane mode/SIM change (stock\n'
     '             default reg_retry_pcscf_policy_on_403=perm_stop): a network that refuses this\n'
     '             device is not hammered, and calls/SMS stay on CS.'),
    ('int-array', 'ims.reg_permanent_err_max_cnt_int_array', [1], None),
]

# Bundles that were verified on a live network. They are frozen: the generator emits exactly
# these keys for the PLMN (plus GLOBAL) instead of the stock-derived ones.
VERIFIED = {
    '24801': {
        'note': 'VERIFIED 2026-11-08 (IMS registered, HD call; Telia EE prepaid)',
        'keys': [
            ('bool', 'carrier_volte_available_bool', True, None),
            ('string', 'ims.ims_user_agent_string', DEFAULT_UA,
             'Verified form (full firmware version); stock would send the last 3 characters.'),
        ],
    },
}

# Networks with no CS domain at all (public operator facts; see VOLTE_A52SXQ_PLAN.md). Without
# IMS there are no calls, no SMS and no emergency calls there, so these get IMS-first emergency
# calls, SMS over IMS (when stock enables smsip) and VoNR (when stock runs MMTEL on NR).
NO_CS = {
    'RJIL_IN': 'Reliance Jio: LTE/NR-only network, never had 2G/3G',
}

# Samsung shortens [BUILD_VERSION] for these operator groups (stock RegistrationManager):
# T-Mobile/Telekom group: last 8 characters; Europe, Middle East, Africa, South/South-East Asia
# and Korea: last 3 characters; everywhere else the full version.
LAST8_MNOS = {'TMobile_DE', 'TMobile_CZ', 'TMobile_PL', 'TMobile_HU', 'TMobile_NL', 'TMobile_GR',
              'TMobile_HR', 'TMobile_SK', 'TMobile_AT', 'TMobile_MK', 'TMobile_RO', 'TMobile_ME',
              'Telekom_AL'}
FULL_COUNTRIES = {'US', 'CA', 'PR', 'AU', 'NZ'}      # by operator country, whatever the MCC
SHORT_COUNTRIES = {'FR', 'MN', 'KR', 'TR', 'EG'}
SWA = {404, 405, 406, 410, 412, 413, 429, 470, 472, 402}
EAST_ASIA = {440, 441, 454, 455, 460, 461, 466}
SEA = {452, 456, 457, 502, 510, 514, 515, 520, 525, 528, 414}

SKIP_MNOS = ('LABSIM', 'GCF', 'DEFAULT', 'GoogleGC', 'TEST')

# ---------------------------------------------------------------------------------------------
# Input parsing
# ---------------------------------------------------------------------------------------------


def strip_json(s):
    """Samsung's JSON has // and /* */ comments and trailing commas."""
    out, i, n, ins = [], 0, len(s), False
    while i < n:
        c = s[i]
        if ins:
            out.append(c)
            if c == '\\':
                out.append(s[i + 1])
                i += 2
                continue
            if c == '"':
                ins = False
            i += 1
            continue
        if c == '"':
            ins = True
        elif s.startswith('//', i):
            j = s.find('\n', i)
            i = n if j < 0 else j
            continue
        elif s.startswith('/*', i):
            i = s.index('*/', i) + 2
            continue
        out.append(c)
        i += 1
    return re.sub(r',(\s*[}\]])', r'\1', ''.join(out))


def read_raw(apk):
    names = ['mnomap', 'imsswitch', 'imsprofile', 'globalsettings']
    data, digests = {}, {}
    with zipfile.ZipFile(apk) as z:
        for n in names:
            b = z.read('res/raw/%s.json' % n)
            digests[n] = hashlib.sha256(b).hexdigest()
            data[n] = json.loads(strip_json(b.decode('utf-8')))
    return data, digests


def parse_carrier_list(path):
    """mccmnc -> [(canonical_id, name)] of carriers that match on MCC/MNC alone, plus
    (mccmnc, GID1 upper) -> [(canonical_id, name)]."""
    text = open(path, encoding='utf-8').read()
    plain, bygid = collections.defaultdict(list), collections.defaultdict(list)
    for blk in re.split(r'\ncarrier_id \{', text)[1:]:
        cid = int(re.search(r'canonical_id: (\d+)', blk).group(1))
        name = re.search(r'carrier_name: "([^"]*)"', blk).group(1)
        for attr in re.findall(r'carrier_attribute \{(.*?)\n  \}', blk, re.S):
            tuples = re.findall(r'mccmnc_tuple: "(\d+)"', attr)
            keys = set(re.findall(r'^\s*(\w+):', attr, re.M)) - {'mccmnc_tuple'}
            gids = re.findall(r'gid1: "([0-9A-Fa-f]+)"', attr)
            for t in tuples:
                if not keys:
                    plain[t].append((cid, name))
                elif keys == {'gid1'}:
                    for g in gids:
                        bygid[(t, g.upper().rstrip('F'))].append((cid, name))
    return plain, bygid


# ---------------------------------------------------------------------------------------------
# Samsung model
# ---------------------------------------------------------------------------------------------


class Stock:
    def __init__(self, raw):
        prof = raw['imsprofile']['profile']
        self.default_profile = prof[0]
        assert self.default_profile['name'] == 'default'
        self.profiles = collections.defaultdict(list)
        for p in prof[1:]:
            self.profiles[p['mnoname'].lower()].append(p)
        sw = raw['imsswitch']
        self.default_switch = sw['defaultswitch']
        self.switch = {}
        for s in sw['imsswitch']:
            self.switch.setdefault(s['mnoname'].lower(), s)
        gs = raw['globalsettings']
        self.default_gs = gs['defaultsetting']
        self.gs = {}
        for g in gs['globalsetting']:
            if 'mnoname' in g:
                self.gs.setdefault(g['mnoname'].lower(), g)
        self.mnomap = raw['mnomap']['mnomap']

    @staticmethod
    def names(mno):
        """Lookup order Samsung uses: the full (MVNO) name, then the primary operator."""
        full = mno.split('@')[0]
        primary = full.split(':')[0]
        return [full.lower()] + ([primary.lower()] if primary != full else [])

    def switch_for(self, mno):
        s = dict(self.default_switch)
        for n in self.names(mno):
            if n in self.switch:
                s.update(self.switch[n])
                break
        return s

    def globals_for(self, mno):
        g = dict(self.default_gs)
        for n in self.names(mno):
            if n in self.gs:
                g.update(self.gs[n])
                break
        return g

    def mmtel_profile(self, mno):
        """(merged profile, own keys, network types) of the operator's normal IMS PDN profile
        with MMTEL on LTE/NR, or None."""
        for n in self.names(mno):
            for p in self.profiles.get(n, []):
                if p.get('pdn', self.default_profile.get('pdn')) != 'ims':
                    continue
                if p.get('enabled', 2) == 0:
                    continue
                for net in p.get('network', []):
                    types = [t.strip() for t in net.get('type', '').split(',')]
                    if (('lte' in types or 'nr' in types) and 'mmtel' in net.get('services', [])
                            and net.get('enabled')):
                        m = dict(self.default_profile)
                        m.update(p)
                        return m, p, types, net.get('services', [])
        return None

    def volte(self, mno):
        """(on, reason, profile tuple)"""
        if any(mno.startswith(s) for s in SKIP_MNOS):
            return False, 'no operator IMS on stock (%s)' % mno, None
        s = self.switch_for(mno)
        if not (s.get('enableIms') and s.get('enableServiceVolte')):
            return False, 'stock imsswitch: VoLTE off', None
        p = self.mmtel_profile(mno)
        if not p:
            return False, 'stock imsswitch on but no MMTEL/LTE profile', None
        return True, 'stock imsswitch+imsprofile: VoLTE on', p


# ---------------------------------------------------------------------------------------------
# Mapping
# ---------------------------------------------------------------------------------------------


def mcc_region(mcc):
    m = int(mcc)
    if 200 <= m < 300:
        return 'EUROPE'
    if 300 <= m < 330:
        return 'NORTH_AMERICA'
    if 330 <= m < 400 or 700 <= m < 800:
        return 'LATIN_AMERICA'
    if 600 <= m < 700:
        return 'AFRICA'
    if m in SWA:
        return 'SOUTH_WEST_ASIA'
    if m in EAST_ASIA:
        return 'EAST_ASIA'
    if m == 450:
        return 'KOREA'
    if m in SEA:
        return 'SOUTH_EAST_ASIA'
    if 400 <= m < 500:
        return 'MIDDLE_EAST'
    return 'OCEANIA'


def build_version_len(mno, mcc):
    primary = mno.split('@')[0].split(':')[0]
    if primary in LAST8_MNOS:
        return 8
    country = primary.rsplit('_', 1)[-1] if '_' in primary else ''
    if country in FULL_COUNTRIES:
        return 0
    if country in SHORT_COUNTRIES:
        return 3
    return 0 if mcc_region(mcc) in ('NORTH_AMERICA', 'LATIN_AMERICA', 'OCEANIA',
                                    'EAST_ASIA') else 3


def translate_ua(fmt, mno, mccmnc, gs):
    """Samsung UA template -> ImsStack template, or (None, reason)."""
    if '#' in fmt:
        return None, 'literal # in stock UA'
    n = build_version_len(mno, mccmnc[:3])
    ver = {0: '#BOOTLOADER#', 8: '#BOOTLOADER_LAST8#', 3: '#BOOTLOADER_LAST3#'}[n]
    primary = mno.split('@')[0].split(':')[0]
    support = 'VoLTE-VVM' if primary in ('TMobile_US', 'Dish_US') else 'VoLTE'
    approx = []
    out = fmt
    if primary == 'TMobile_US':
        # Stock: PRD-IR92/13 on LTE, the RCS client configuration (UP_2.3 when none) and
        # "cc/<entitlement config version>", which only exists with T-Mobile's entitlement app.
        out = out.replace('[IMS_PROFILE_VOICE]', 'PRD-IR92/13').replace('[RCS_PROFILE]', 'UP_2.3')
        if '[MNO_CUSTOM]' in out:
            out = re.sub(r'\s*\[MNO_CUSTOM\]', '', out)
            approx.append('[MNO_CUSTOM]')
    # The sales/network code of the device's CSC and the Korean UICC version have no equivalent
    # here: dropped (with the space before them), the rest of the format is kept.
    for ph in ('[OMCCODE]', '[UICC_VERSION]'):
        if ph in out:
            out = re.sub(r'[ /]?' + re.escape(ph), '', out)
            approx.append(ph)
    rep = {
        '[PRODUCT_MODEL]': '#MODEL#',
        '[BUILD_VERSION]': ver,
        '[OS_VERSION]': '#AV#',
        '[IMEISV]': '#IMEISV#',
        '[CLIENT_VERSION]': str(gs.get('rcs_client_version') or '6.0'),
        '[SUPPORT]': support,
        '[PRODUCT_TYPE]': 'device-type/smart-phone',
        '[MNO_CUSTOM]': 'mno-custom/' + mccmnc,
        '[MCCMNC]': mccmnc,
    }
    for k, v in rep.items():
        out = out.replace(k, v)
    left = re.findall(r'\[[A-Z_]+\]', out)
    if left:
        return None, 'stock UA uses %s' % ','.join(left)
    return out, ('UA without ' + ','.join(approx)) if approx else None


AUTH = {'hmac-md5-96': 0, 'hmac-sha-1-96': 1}
ENC = {'null': 0, 'des-ede3-cbc': 1, 'aes-cbc': 2}
TRANSPORT = {'udp': 0, 'tcp': 1, 'udp-preferred': 2}
TIMERS = {'1': ('t1', 2000), '2': ('t2', 16000), '4': ('t4', 17000), 'B': ('b', 128000),
          'C': ('c', 210000), 'D': ('d', 130000), 'F': ('f', 128000), 'H': ('h', 128000),
          'J': ('j', 128000)}


def boolv(v):
    return v is True or str(v).lower() == 'true'


def operator_keys(mno, mccmnc, prof, stock):
    """CarrierConfig keys for a VoLTE operator, from its stock profile. Keys equal to GLOBAL or to
    the AOSP/ImsStack default are left out. Returns (keys, notes)."""
    m, own, types, services = prof
    gs = stock.globals_for(mno)
    sw = stock.switch_for(mno)
    keys, notes = [('bool', 'carrier_volte_available_bool', True, None)], []

    ipsec = boolv(m.get('support_ipsec'))
    if not ipsec:
        keys.append(('bool', 'ims.sip_over_ipsec_enabled_bool', False, None))
    else:
        a = [AUTH[x.strip()] for x in str(m.get('auth_algo', '')).split(',') if x.strip() in AUTH]
        if a and a != [0, 1]:
            keys.append(('int-array', 'ims.ipsec_authentication_algorithms_int_array', a, None))
        e = [ENC[x.strip()] for x in str(m.get('enc_algo', '')).split(',') if x.strip() in ENC]
        if e and sorted(e) != [0, 1, 2]:
            keys.append(('int-array', 'ims.ipsec_encryption_algorithms_int_array', e, None))

    t = TRANSPORT.get(str(m.get('transport', 'udp-preferred')))
    if t is None:
        notes.append('transport %s not mapped' % m.get('transport'))
    elif t != 2:
        keys.append(('int', 'ims.sip_preferred_transport_int', t, None))
    port = int(m.get('port', 5060))
    if port != 5060:
        keys.append(('int', 'ims.sip_server_port_number_int', port, None))

    if not boolv(m.get('use_precondition')):
        keys.append(('bool', 'imsvoice.voice_qos_precondition_supported_bool', False, None))

    mss = int(m.get('mss_size', 1500))
    mtu = min(1440, mss + 140) if mss < 1500 else 1440
    if mtu != 1440:
        keys.append(('int', 'ims.max_allowed_network_mtu_int', mtu,
                     'stock mss_size=%d' % mss))

    # Values below only when the operator's own profile sets them (Samsung's defaults for these
    # differ from AOSP's without being operator facts).
    if 'enable_gruu' in own:
        if boolv(own['enable_gruu']):
            keys.append(('bool', 'ims.gruu_enabled_bool', True, None))
    if 'remote_uri_type' in own and own['remote_uri_type'] == 'sip':
        keys.append(('int', 'ims.request_uri_type_int', 1, None))
    if 'reg_expires' in own and int(own['reg_expires']) not in (600000,):
        keys.append(('int', 'ims.registration_expiry_timer_sec_int', int(own['reg_expires']),
                     None))
    if 'session_expires' in own:
        se = int(own['session_expires'])
        if se == 0:
            keys.append(('bool', 'imsvoice.session_timer_supported_bool', False, None))
        elif se != 1800:
            keys.append(('int', 'imsvoice.session_expires_timer_sec_int', se, None))
    if 'min_se' in own and int(own['min_se']) != 90:
        keys.append(('int', 'imsvoice.minimum_session_expires_timer_sec_int', int(own['min_se']),
                     None))
    for k, aosp in (('ringing_timer', 'imsvoice.ringing_timer_millis_int'),
                    ('ringback_timer', 'imsvoice.ringback_timer_millis_int')):
        if k in own and int(own[k]) * 1000 != 90000 and int(own[k]) > 0:
            keys.append(('int', aosp, int(own[k]) * 1000, None))
    if 'timer' in own:
        for item in str(own['timer']).split(','):
            if ':' not in item:
                continue
            tk, tv = item.split(':', 1)
            if tk in TIMERS and int(tv) != TIMERS[tk][1]:
                keys.append(('int', 'ims.sip_timer_%s_millis_int' % TIMERS[tk][0], int(tv), None))

    policy403 = str(m.get('reg_retry_pcscf_policy_on_403', 'perm_stop'))
    if policy403 in ('next_pcscf', 'same_pcscf'):
        keys.append(('int-array', 'ims.reg_permanent_err_max_cnt_int_array', [2],
                     'stock retries 403 on %s once' % policy403.replace('_', ' ')))

    ua = m.get('useragent') or stock.default_profile.get('useragent')
    tua, why = translate_ua(ua, mno, mccmnc, gs)
    if why:
        notes.append(why if tua else 'UA default (%s)' % why)
    if tua is not None and tua != DEFAULT_UA:
        keys.append(('string', 'ims.ims_user_agent_string', tua, None))

    primary = mno.split('@')[0].split(':')[0]
    if primary in NO_CS:
        keys.append(('bool', 'carrier_use_ims_first_for_emergency_bool', True,
                     'no CS domain: %s' % NO_CS[primary]))
        rats = [EUTRAN] + ([NGRAN] if 'nr' in types else [])
        if sw.get('enableServiceSmsip') and 'smsip' in services:
            keys.append(('bool', 'imssms.sms_over_ims_supported_bool', True, None))
            keys.append(('int-array', 'imssms.sms_over_ims_supported_rats_int_array', rats,
                         None))
        if 'nr' in types:
            keys.append(('bool', 'vonr_enabled_bool', True,
                         'stock runs MMTEL on NR; on NR SA voice has no other path'))
            keys.append(('int-array', 'imsemergency.emergency_over_ims_supported_3gpp_network_'
                         'types_int_array', rats, None))
        notes.append('NO_CS: IMS-first emergency, SMS over IMS, VoNR')
    return keys, notes


# ---------------------------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------------------------


def xesc(s):
    return (s.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')
            .replace('"', '&quot;'))


def render_key(k, indent='        '):
    typ, name, val, comment = k
    out = []
    if comment:
        out.append('%s<!-- %s -->' % (indent, comment.replace('--', '-')))
    if typ == 'bool':
        out.append('%s<boolean name="%s" value="%s" />' % (indent, name, 'true' if val else 'false'))
    elif typ == 'int':
        out.append('%s<int name="%s" value="%d" />' % (indent, name, val))
    elif typ == 'string':
        # CarrierConfig reads <string> as element text (a value= attribute is ignored).
        out.append('%s<string name="%s">%s</string>' % (indent, name, xesc(val)))
    elif typ == 'int-array':
        out.append('%s<int-array name="%s" num="%d">' % (indent, name, len(val)))
        for v in val:
            out.append('%s    <item value="%d" />' % (indent, v))
        out.append('%s</int-array>' % indent)
    return out


def filt(attrs):
    return ''.join(' %s="%s"' % (k, xesc(v)) for k, v in attrs)


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--imsservice', required=True)
    ap.add_argument('--stock-apns', required=True)
    ap.add_argument('--check', action='store_true',
                    help='only compare with the checked-in outputs (exit 1 if they differ)')
    args = ap.parse_args(argv)

    raw, digests = read_raw(args.imsservice)
    stock = Stock(raw)
    plain_cids, gid_cids = parse_carrier_list(CARRIER_LIST)
    aosp_text = open(AOSP_APNS, encoding='utf-8').read()
    stock_apns_text = open(args.stock_apns, encoding='utf-8', errors='replace').read()
    aosp_ims = apnmerge.ims_apns(aosp_text)
    stock_ims = apnmerge.ims_apns(stock_apns_text)
    stock_apns_digest = hashlib.sha256(stock_apns_text.encode('utf-8', 'replace')).hexdigest()

    # mnomap: per PLMN the plain entry and the more specific (MVNO) ones.
    byplmn = collections.OrderedDict()
    for e in stock.mnomap:
        mm = e['mccmnc']
        if not re.fullmatch(r'\d{5,6}', mm) or mm[:3] in ('001', '999'):
            continue
        byplmn.setdefault(mm, []).append(e)

    def aosp_name(mm):
        n = sorted(set(x[1] for x in plain_cids.get(mm, [])))
        return ' / '.join(n)

    base_bundles, specific_bundles, rows = [], [], []
    apn_overrides = []
    stats = collections.Counter()
    countries_on = set()

    for mm in sorted(byplmn):
        mcc, mnc = mm[:3], mm[3:]
        entries = byplmn[mm]
        plain = [e for e in entries if not (e['subset'] or e['gid1'] or e['gid2'] or e['spname'])]
        base = plain[0] if plain else None
        base_on = False
        base_keys = None
        if base:
            mno = base['mnoname']
            if mm in VERIFIED:
                on, reason, prof = True, VERIFIED[mm]['note'], stock.volte(mno)[2]
                keys, notes = VERIFIED[mm]['keys'], []
            else:
                on, reason, prof = stock.volte(mno)
                keys, notes = operator_keys(mno, mm, prof, stock) if on else ([], [])
            base_on, base_keys = on, keys
            status = 'verified' if mm in VERIFIED else ('generated' if on else 'off')
            stats[status] += 1
            if on:
                countries_on.add(mno.split('@')[0].split(':')[0].rsplit('_', 1)[-1])
                title = '%s %s-%s %s' % (mno.split('@')[0], mcc, mnc,
                                         ('(AOSP: %s)' % aosp_name(mm)) if aosp_name(mm) else '')
                lines = ['    <!-- %s%s -->' % (title.strip().replace('--', '-'),
                                                 ('; ' + reason) if mm in VERIFIED else '')]
                lines.append('    <carrier_config%s>' % filt([('mcc', mcc), ('mnc', mnc)]))
                for k in keys:
                    lines += render_key(k)
                lines.append('    </carrier_config>')
                base_bundles.append('\n'.join(lines))
                ua = next((k[2] for k in keys if k[1] == 'ims.ims_user_agent_string'), DEFAULT_UA)
                apn_overrides += apn_actions(mcc, mnc, aosp_ims, stock_ims, plain_cids, mm)
            else:
                ua = ''
            rows.append([mm, mno.split('@')[0], aosp_name(mm), status, reason, ua,
                         '; '.join(notes)])

        for e in entries:
            if e is base:
                continue
            mno = e['mnoname']
            on, reason, prof = stock.volte(mno)
            keys, notes = operator_keys(mno, mm, prof, stock) if on else ([], [])
            if on == base_on and (not on or keys == base_keys):
                continue  # same result as the PLMN's own bundle
            f, how = specific_filter(mm, e, gid_cids)
            what = 'subset=%s gid1=%s gid2=%s spn=%s' % (e['subset'], e['gid1'], e['gid2'],
                                                          e['spname'])
            if f is None:
                stats['mvno-unmapped'] += 1
                rows.append([mm, mno.split('@')[0], what, 'unmapped', how,
                             '', 'stock: ' + reason])
                continue
            stats['mvno-' + ('on' if on else 'off')] += 1
            lines = ['    <!-- %s on %s-%s (%s) -->' % (mno.split('@')[0], mcc, mnc,
                                                      how.replace('--', '-'))]
            lines.append('    <carrier_config%s>' % filt(f))
            if on:
                for k in keys:
                    lines += render_key(k)
            else:
                lines += render_key(('bool', 'carrier_volte_available_bool', False, None))
            lines.append('    </carrier_config>')
            specific_bundles.append('\n'.join(lines))
            rows.append([mm, mno.split('@')[0], what, 'mvno-' + ('on' if on else 'off'), reason,
                         '', '; '.join(notes)])

    vendor = render_vendor(digests, stock_apns_digest, base_bundles, specific_bundles, stats,
                           len(countries_on))
    apns = render_apns(apn_overrides, digests, stock_apns_digest)
    tsv = '\t'.join(['plmn', 'stock_mno', 'aosp_carrier_or_filter', 'status', 'reason',
                     'user_agent', 'notes']) + '\n'
    tsv += ''.join('\t'.join(r) + '\n' for r in rows)

    # Sanity: the merged APN list must apply cleanly to today's AOSP list.
    import xml.etree.ElementTree as ET
    apnmerge.merge(aosp_text, list(ET.fromstring(apns)))

    outs = [(OUT_VENDOR, vendor), (OUT_APNS, apns), (OUT_TSV, tsv)]
    if args.check:
        bad = [p for p, c in outs if not os.path.exists(p) or open(p).read() != c]
        for p in bad:
            print('out of date: ' + os.path.relpath(p, TREE))
        return 1 if bad else 0
    for p, c in outs:
        with open(p, 'w') as f:
            f.write(c)
    print('VoLTE PLMNs: %d generated + %d verified, %d off; MVNO bundles: %d on, %d off, %d '
          'unmapped; %d countries; %d APN overrides' % (
              stats['generated'], stats['verified'], stats['off'], stats['mvno-on'],
              stats['mvno-off'], stats['mvno-unmapped'], len(countries_on), len(apn_overrides)))
    return 0


def specific_filter(mm, e, gid_cids):
    """CarrierConfig filter for a Samsung MVNO entry, or (None, why)."""
    mcc, mnc = mm[:3], mm[3:]
    f = [('mcc', mcc), ('mnc', mnc)]
    how = []
    if e['gid2']:
        return None, 'GID2 match has no CarrierConfig equivalent'
    if e['subset']:
        sub = e['subset']
        lead = len(sub) - len(sub.lstrip('xX'))
        f.append(('imsi', mm + '.' * lead + sub[lead:] + '.*'))
        how.append('IMSI ' + mm + sub)
    if e['spname']:
        # Regex (CarrierConfig matches it case-insensitively against the whole SPN); no
        # backslashes, so aapt2 can't reinterpret anything: specials go in [] classes.
        lit = ''.join(c if c.isalnum() or c == ' ' else '[%s]' % c for c in e['spname'].strip())
        f.append(('spn', ' *%s *' % lit))
        how.append('SPN ' + e['spname'].strip())
    if e['gid1']:
        g = e['gid1'].upper().rstrip('F')
        cids = gid_cids.get((mm, g), [])
        if len(set(c[0] for c in cids)) == 1:
            f.append(('cid', str(cids[0][0])))
            how.append('GID1 %s = AOSP carrier id %d %s' % (e['gid1'], cids[0][0], cids[0][1]))
        elif len(e['gid1']) >= 16:
            f.append(('gid1', e['gid1']))
            how.append('GID1 ' + e['gid1'])
        else:
            return None, 'short GID1 %s (prefix match) and no AOSP carrier id' % e['gid1']
    return f, ', '.join(how)


def apn_actions(mcc, mnc, aosp_ims, stock_ims, plain_cids, mm):
    a = apnmerge.find(aosp_ims, mcc, mnc)
    s = apnmerge.find(stock_ims, mcc, mnc)
    if not s:
        return []  # no stock IMS APN: telephony's default IMS APN ("ims", IPV4V6) applies
    sa = s[0][1]
    out = []
    if not a:
        # Telephony's default IMS APN is "ims" with IPV4V6, so only a differently named stock APN
        # needs an entry; it asks for IPV4V6 too (never narrower than the default would).
        if sa.get('apn', 'ims').lower() == 'ims':
            return []
        add = {'carrier': sa.get('carrier', 'IMS'), 'mcc': mcc, 'mnc': mnc,
               'apn': sa['apn'], 'type': 'ims', 'protocol': 'IPV4V6',
               'roaming_protocol': 'IPV4V6', 'user_visible': 'false'}
        cids = sorted(set(c[0] for c in plain_cids.get(mm, [])))
        if len(cids) == 1:
            add['carrier_id'] = str(cids[0])
        out.append(('add', add))
        return out
    if len(a) != 1:
        return []
    aa = a[0][1]
    for attr in ('protocol', 'roaming_protocol'):
        cur, want = aa.get(attr, ''), sa.get(attr, '')
        # Never narrow: only widen to dual stack where AOSP asks for less than stock does.
        if want and cur != want and cur != 'IPV4V6':
            out.append(('fix', {'mcc': mcc, 'mnc': mnc, 'apn': aa.get('apn', ''), 'attr': attr,
                                'from': cur, 'to': 'IPV4V6'}))
    return out


def header(digests, stock_apns_digest):
    return ('     GENERATED by ims/tools/gen_carrier_ims.py - do not edit; change the generator\n'
            '     and rerun it (see VOLTE_A52SXQ_PLAN.md, "Carrier data").\n'
            '     Inputs (SM-A528B stock firmware): imsservice.apk res/raw\n' +
            ''.join('       %s.json sha256 %s\n' % (k, v) for k, v in sorted(digests.items())) +
            '       apns-conf.xml sha256 %s\n' % stock_apns_digest)


def render_vendor(digests, sad, base, specific, stats, ncountries):
    out = ['<?xml version="1.0" encoding="utf-8" standalone="yes"?>', '<!--',
           '     Copyright (C) 2026 IchthysOS', '     SPDX-License-Identifier: Apache-2.0', '',
           header(digests, sad),
           '     IchthysOS IMS gating. CarrierConfig reads this after the per-carrier asset file,'
           ' bundle by\n     bundle, every matching bundle applied in order (later ones win):\n'
           '       1. global defaults (no filter): every SIM, including operators stock does not'
           ' know;\n'
           '       2. one bundle per PLMN on which the stock firmware turns VoLTE on (%d PLMNs,'
           ' %d countries;\n'
           '          %d of them verified on a live network);\n'
           '       3. MVNO bundles (IMSI range / SPN / GID1 / carrier id) where stock treats an'
           ' MVNO\n'
           '          differently from its host network (%d).\n'
           '     Per-PLMN bundles only list keys that differ from the global ones or from the'
           ' AOSP/ImsStack\n     defaults.' % (stats['generated'] + stats['verified'], ncountries,
                                                stats['verified'], len(specific)),
           '-->', '<carrier_config_list>', '    <carrier_config>']
    for k in GLOBAL:
        out += render_key(k)
    out.append('    </carrier_config>')
    out.append('')
    out += [b + '\n' for b in base]
    out += [b + '\n' for b in specific]
    out.append('</carrier_config_list>')
    return '\n'.join(out).rstrip('\n') + '\n'


def render_apns(actions, digests, sad):
    out = ['<?xml version="1.0" encoding="utf-8"?>', '<!--',
           '     Copyright (C) 2026 IchthysOS', '     SPDX-License-Identifier: Apache-2.0', '',
           header(digests, sad),
           '     IMS APN changes on top of the AOSP list for operators that get VoLTE, applied by'
           '\n     ims_apns_merge.py at build time:\n'
           '       fix: the AOSP IMS APN asks for less IP stack than stock does -> IPV4V6 (never'
           ' narrowed)\n'
           '       add: AOSP has no IMS APN and stock names it other than "ims" -> stock\'s APN'
           ' name,\n'
           '            IPV4V6. Otherwise telephony brings up its default IMS APN ("ims",'
           ' IPV4V6).',
           '-->', '<ims_apns>']
    for kind, a in actions:
        order = ['mcc', 'mnc', 'apn', 'attr', 'from', 'to'] if kind == 'fix' else \
            ['carrier', 'carrier_id', 'mcc', 'mnc', 'apn', 'type', 'protocol',
             'roaming_protocol', 'user_visible']
        out.append('    <%s %s />' % (kind, ' '.join('%s="%s"' % (k, xesc(a[k]))
                                                     for k in order if k in a)))
    out.append('</ims_apns>')
    return '\n'.join(out) + '\n'


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
