//! The MIME message parser (mime/parser.ts).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Result;
use serde::{Serialize, Serializer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::address::{parse_address_header, OneOrMany, ParsedAddress};
use super::charset::{decode_body_bytes, jsstr, utf8_lossy};
use super::header::{
    decode_words, ensure_message_id_format, parse_header_value, parse_message_id_list, raw_octets_as_utf8,
    split_header_line,
};
use super::text::{embedded_headers_as_html, embedded_headers_as_text, text_to_html, EmbeddedHeaders};
use super::transfer_decoder::{create_transfer_decoder, TransferDecoder};
use crate::mails::plain_text::{html_to_plain_text, PlainTextError, PlainTextOptions};

pub use super::text::MessageDate;

impl Serialize for MessageDate {
    /// As `JSON.stringify` writes a `Date`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_iso_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedAttachment {
    /// Where the decoded content was written. The caller owns the file.
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub content_type: String,
    pub size: u64,
    /// Content-ID without its angle brackets, when the part has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// The Message-ID this mail answers (`In-Reply-To`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// The conversation's Message-IDs, oldest first (`References`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<ParsedAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<OneOrMany<ParsedAddress>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cc: Option<OneOrMany<ParsedAddress>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bcc: Option<OneOrMany<ParsedAddress>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ParsedAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<MessageDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    pub attachments: Vec<ParsedAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOptions {
    /// Directory attachment files are written into; created on first use.
    pub attachment_dir: PathBuf,
}

/// Largest header block of a single MIME part.
pub const MAX_PART_HEADER_BYTES: usize = 1024 * 1024;

/// Most MIME parts one message may hold.
pub const MAX_PARTS: usize = 1000;

/// Decoded text and HTML body parts held in memory, summed over the message.
/// A part that would cross it is stored as an attachment instead, so a
/// hostile multi-megabyte body costs disk rather than heap.
pub const TEXT_BODY_BUDGET_BYTES: usize = 2 * 1024 * 1024;

/// Most HTML the plain-text rendering of an HTML-only message is computed
/// from. The renderer's working set is tens of times its input, so a longer
/// body is rendered from its first part.
pub const HTML_TO_TEXT_INPUT_LIMIT: usize = 256 * 1024;

/// Most image bytes inlined into the HTML as `data:` URLs in place of `cid:`
/// references. Past it the reference is left as written and the image stays
/// an attachment.
pub const INLINE_IMAGE_BUDGET_BYTES: u64 = 4 * 1024 * 1024;

/// Why a message is refused.
#[derive(Debug, thiserror::Error)]
pub enum MimeParseError {
    #[error("Max header size for a MIME node exceeded")]
    HeaderTooLarge,
    #[error("Max allowed child nodes exceeded")]
    TooManyParts,
    /// A file name whose extension holds a NUL: looking up its type fails.
    #[error("The argument 'path' must be a string, Uint8Array, or URL without null bytes. Received {0}")]
    FilenameWithNul(String),
    /// A file name whose extension is as long as a path may be: looking up
    /// its type fails.
    #[error("{0}")]
    FilenameTooLong(String),
    /// HTML the plain-text renderer fails on.
    #[error(transparent)]
    PlainText(#[from] PlainTextError),
}

fn is_text_type(content_type: &str) -> bool {
    matches!(content_type, "text/plain" | "text/html" | "message/delivery-status")
}

/// A header line of one part: its unfolded, lowercased key and the raw
/// (folded) line, each byte of it a character.
struct RawHeader {
    key: String,
    line: String,
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

/// `Buffer.from(text, "latin1")`: the low byte of every UTF-16 code unit.
fn latin1_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut units = [0u16; 2];
    for c in text.chars() {
        bytes.extend(c.encode_utf16(&mut units).iter().map(|unit| *unit as u8));
    }
    bytes
}

/// The pieces of `bytes` between its `\r?\n` line breaks.
fn split_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pieces = bytes.split(|&byte| byte == b'\n').peekable();
    std::iter::from_fn(move || {
        let piece = pieces.next()?;
        Some(match pieces.peek() {
            Some(_) => piece.strip_suffix(b"\r").unwrap_or(piece),
            None => piece,
        })
    })
}

/// Removes everything from the first `(` to the last `)`, as an RFC 5322 comment.
fn without_comment(value: &str) -> String {
    match (value.find('('), value.rfind(')')) {
        (Some(open), Some(close)) if close > open => format!("{}{}", &value[..open], &value[close + 1..]),
        _ => value.to_string(),
    }
}

fn split_header_block(block: &[u8]) -> Vec<RawHeader> {
    let mut end = block.len();
    while end > 0 && matches!(block[end - 1], b'\r' | b'\n') {
        end -= 1;
    }
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for line in split_lines(&block[..end]) {
        match lines.last_mut() {
            Some(last) if matches!(line.first(), Some(b' ' | b'\t')) => {
                last.extend_from_slice(b"\r\n");
                last.extend_from_slice(line);
            }
            _ => lines.push(line.to_vec()),
        }
    }
    // An mbox separator line is not a header.
    let from_line = lines
        .first()
        .is_some_and(|first| first.len() >= 5 && first[..5].eq_ignore_ascii_case(b"From "));
    lines
        .iter()
        .skip(usize::from(from_line))
        .filter(|line| !line.is_empty())
        .map(|line| {
            let colon = line.iter().position(|&byte| byte == b':').unwrap_or(0);
            RawHeader {
                key: jsstr::trim(&latin1(&line[..colon])).to_lowercase(),
                line: latin1(line),
            }
        })
        .collect()
}

fn first_header(lines: &[RawHeader], key: &str) -> String {
    match lines.iter().find(|line| line.key == key) {
        Some(found) => split_header_line(&found.line).value,
        None => String::new(),
    }
}

/// A header value as text: the raw octets read as UTF-8.
fn header_text(line: &str) -> String {
    utf8_lossy(&latin1_bytes(&split_header_line(line).value))
}

#[derive(Default)]
struct MessageHeaders {
    embedded: EmbeddedHeaders,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references: Option<Vec<String>>,
    reply_to: Option<ParsedAddress>,
    content_id: Option<String>,
}

fn one_or_many(mut list: Vec<ParsedAddress>) -> Option<OneOrMany<ParsedAddress>> {
    match list.len() {
        0 => None,
        1 => list.pop().map(OneOrMany::One),
        _ => Some(OneOrMany::Many(list)),
    }
}

/// The headers this parser reports, decoded: a repeated single-valued header
/// keeps its last occurrence, and repeated `To`/`Cc`/`Bcc` become a list.
fn read_message_headers(lines: &[RawHeader]) -> MessageHeaders {
    let mut headers = MessageHeaders::default();
    let (mut to, mut cc, mut bcc) = (Vec::new(), Vec::new(), Vec::new());
    for RawHeader { key, line } in lines {
        let known = matches!(
            key.as_str(),
            "from"
                | "to"
                | "cc"
                | "bcc"
                | "reply-to"
                | "subject"
                | "date"
                | "message-id"
                | "content-id"
                | "in-reply-to"
                | "references"
        );
        if !known {
            continue;
        }
        let text = header_text(line);
        match key.as_str() {
            "from" => headers.embedded.from = Some(parse_address_header(&text)),
            "to" => to.push(parse_address_header(&text)),
            "cc" => cc.push(parse_address_header(&text)),
            "bcc" => bcc.push(parse_address_header(&text)),
            "reply-to" => headers.reply_to = Some(parse_address_header(&text)),
            "subject" => {
                let subject = decode_words(&text);
                if !subject.is_empty() {
                    headers.embedded.subject = Some(subject);
                }
            }
            // A date that cannot be read is the time of reading.
            "date" => {
                headers.embedded.date = Some(
                    date_parse(&text)
                        .and_then(MessageDate::from_millis)
                        .unwrap_or_else(MessageDate::now),
                );
            }
            "message-id" => {
                if let Some(id) = ensure_message_id_format(&decode_words(&text)) {
                    headers.message_id = Some(id);
                }
            }
            "content-id" => {
                if !text.is_empty() {
                    headers.content_id = Some(text);
                }
            }
            // In-Reply-To names one message; a list there is malformed, so
            // its last id stands for the parent.
            "in-reply-to" => {
                if let Some(id) = parse_message_id_list(&text).pop() {
                    headers.in_reply_to = Some(id);
                }
            }
            _ => {
                let ids = parse_message_id_list(&text);
                if !ids.is_empty() {
                    headers.references = Some(ids);
                }
            }
        }
    }
    headers.embedded.to = one_or_many(to);
    headers.embedded.cc = one_or_many(cc);
    headers.embedded.bcc = one_or_many(bcc);
    headers
}

/// Bun's table of file types by extension (`Bun.file(name).type`): entries of
/// a MIME type followed by the extensions that have it.
const MIME_TYPE_TABLE: &str = "\
    application/andrew-inset ez;application/applixware aw;application/atom+xml atom;\
    application/atomcat+xml atomcat;application/atomdeleted+xml atomdeleted;application/atomsvc+xml atomsvc;\
    application/atsc-dwd+xml dwd;application/atsc-held+xml held;application/atsc-rsat+xml rsat;\
    application/calendar+xml xcs;application/ccxml+xml ccxml;application/cdfx+xml cdfx;\
    application/cdmi-capability cdmia;application/cdmi-container cdmic;application/cdmi-domain cdmid;\
    application/cdmi-object cdmio;application/cdmi-queue cdmiq;application/cpl+xml cpl;\
    application/cu-seeme cu;application/dash+xml mpd;application/davmount+xml davmount;\
    application/docbook+xml dbk;application/dssc+der dssc;application/dssc+xml xdssc;\
    application/ecmascript ecma es;application/emma+xml emma;application/emotionml+xml emotionml;\
    application/epub+zip epub;application/exi exi;application/express exp;application/fdt+xml fdt;\
    application/font-tdpfr pfr;application/geo+json geojson;application/gml+xml gml;application/gpx+xml gpx;\
    application/gxf gxf;application/gzip gz;application/hjson hjson;application/hyperstudio stk;\
    application/inkml+xml ink inkml;application/ipfix ipfix;application/its+xml its;\
    application/java-archive ear jar war;application/java-serialized-object ser;application/java-vm class;\
    application/json json map;application/json5 json5;application/jsonml+json jsonml;\
    application/ld+json jsonld;application/lgr+xml lgr;application/lost+xml lostxml;\
    application/mac-binhex40 hqx;application/mac-compactpro cpt;application/mads+xml mads;\
    application/manifest+json webmanifest;application/marc mrc;application/marcxml+xml mrcx;\
    application/mathematica ma mb nb;application/mathml+xml mathml;application/mbox mbox;\
    application/media-policy-dataset+xml mpf;application/mediaservercontrol+xml mscml;\
    application/metalink+xml metalink;application/metalink4+xml meta4;application/mets+xml mets;\
    application/mmt-aei+xml maei;application/mmt-usd+xml musd;application/mods+xml mods;\
    application/mp21 m21 mp21;application/mp4 m4p mp4s;application/msword doc dot;application/mxf mxf;\
    application/n-quads nq;application/n-triples nt;application/oda oda;application/oebps-package+xml opf;\
    application/ogg ogx;application/omdoc+xml omdoc;application/onenote onepkg onetmp onetoc onetoc2;\
    application/oxps oxps;application/p2p-overlay+xml relo;application/patch-ops-error+xml xer;\
    application/pdf pdf;application/pgp-encrypted pgp;application/pgp-signature asc sig;\
    application/pics-rules prf;application/pkcs10 p10;application/pkcs7-mime p7c p7m;\
    application/pkcs7-signature p7s;application/pkcs8 p8;application/pkix-cert cer;application/pkix-crl crl;\
    application/pkix-pkipath pkipath;application/pkixcmp pki;application/pls+xml pls;\
    application/postscript ai eps ps;application/provenance+xml provx;application/prs.cww cww;\
    application/pskc+xml pskcxml;application/raml+yaml raml;application/rdf+xml owl rdf;\
    application/reginfo+xml rif;application/relax-ng-compact-syntax rnc;application/resource-lists+xml rl;\
    application/resource-lists-diff+xml rld;application/rls-services+xml rs;application/route-apd+xml rapd;\
    application/route-s-tsid+xml sls;application/route-usd+xml rusd;application/rpki-ghostbusters gbr;\
    application/rpki-manifest mft;application/rpki-roa roa;application/rsd+xml rsd;application/rss+xml rss;\
    application/sbml+xml sbml;application/scvp-cv-request scq;application/scvp-cv-response scs;\
    application/scvp-vp-request spq;application/scvp-vp-response spp;application/sdp sdp;\
    application/senml+xml senmlx;application/sensml+xml sensmlx;application/set-payment-initiation setpay;\
    application/set-registration-initiation setreg;application/shf+xml shf;application/sieve sieve siv;\
    application/smil+xml smi smil;application/sparql-query rq;application/sparql-results+xml srx;\
    application/srgs gram;application/srgs+xml grxml;application/sru+xml sru;application/ssdl+xml ssdl;\
    application/ssml+xml ssml;application/swid+xml swidtag;application/tei+xml tei teicorpus;\
    application/thraud+xml tfi;application/timestamped-data tsd;application/toml toml;application/trig trig;\
    application/ttml+xml ttml;application/ubjson ubj;application/urc-ressheet+xml rsheet;\
    application/urc-targetdesc+xml td;application/vnd.1000minds.decision-model+xml 1km;\
    application/vnd.3gpp.pic-bw-large plb;application/vnd.3gpp.pic-bw-small psb;\
    application/vnd.3gpp.pic-bw-var pvb;application/vnd.3gpp2.tcap tcap;\
    application/vnd.3m.post-it-notes pwn;application/vnd.accpac.simply.aso aso;\
    application/vnd.accpac.simply.imp imp;application/vnd.acucobol acu;application/vnd.acucorp acutc atc;\
    application/vnd.adobe.air-application-installer-package+zip air;\
    application/vnd.adobe.formscentral.fcdt fcdt;application/vnd.adobe.fxp fxp fxpl;\
    application/vnd.adobe.xdp+xml xdp;application/vnd.adobe.xfdf xfdf;application/vnd.age age;\
    application/vnd.ahead.space ahead;application/vnd.airzip.filesecure.azf azf;\
    application/vnd.airzip.filesecure.azs azs;application/vnd.amazon.ebook azw;\
    application/vnd.americandynamics.acc acc;application/vnd.amiga.ami ami;\
    application/vnd.android.package-archive apk;application/vnd.anser-web-certificate-issue-initiation cii;\
    application/vnd.anser-web-funds-transfer-initiation fti;application/vnd.antix.game-component atx;\
    application/vnd.apple.installer+xml mpkg;application/vnd.apple.mpegurl m3u8;\
    application/vnd.apple.pkpass pkpass;application/vnd.aristanetworks.swi swi;\
    application/vnd.astraea-software.iota iota;application/vnd.audiograph aep;\
    application/vnd.balsamiq.bmml+xml bmml;application/vnd.blueice.multipass mpm;application/vnd.bmi bmi;\
    application/vnd.businessobjects rep;application/vnd.chemdraw+xml cdxml;\
    application/vnd.chipnuts.karaoke-mmd mmd;application/vnd.cinderella cdy;\
    application/vnd.citationstyles.style+xml csl;application/vnd.claymore cla;\
    application/vnd.cloanto.rp9 rp9;application/vnd.clonk.c4group c4d c4f c4g c4p c4u;\
    application/vnd.cluetrust.cartomobile-config c11amc;\
    application/vnd.cluetrust.cartomobile-config-pkg c11amz;application/vnd.commonspace csp;\
    application/vnd.contact.cmsg cdbcmsg;application/vnd.cosmocaller cmc;application/vnd.crick.clicker clkx;\
    application/vnd.crick.clicker.keyboard clkk;application/vnd.crick.clicker.palette clkp;\
    application/vnd.crick.clicker.template clkt;application/vnd.crick.clicker.wordbank clkw;\
    application/vnd.criticaltools.wbs+xml wbs;application/vnd.ctc-posml pml;application/vnd.cups-ppd ppd;\
    application/vnd.curl.car car;application/vnd.curl.pcurl pcurl;application/vnd.dart dart;\
    application/vnd.data-vision.rdz rdz;application/vnd.dbf dbf;application/vnd.dece.data uvd uvf uvvd uvvf;\
    application/vnd.dece.ttml+xml uvt uvvt;application/vnd.dece.unspecified uvvx uvx;\
    application/vnd.dece.zip uvvz uvz;application/vnd.denovo.fcselayout-link fe_launch;\
    application/vnd.dna dna;application/vnd.dolby.mlp mlp;application/vnd.dpgraph dpg;\
    application/vnd.dreamfactory dfac;application/vnd.ds-keypoint kpxx;application/vnd.dvb.ait ait;\
    application/vnd.dvb.service svc;application/vnd.dynageo geo;application/vnd.ecowin.chart mag;\
    application/vnd.enliven nml;application/vnd.epson.esf esf;application/vnd.epson.msf msf;\
    application/vnd.epson.quickanime qam;application/vnd.epson.salt slt;application/vnd.epson.ssf ssf;\
    application/vnd.eszigno3+xml es3 et3;application/vnd.ezpix-album ez2;application/vnd.ezpix-package ez3;\
    application/vnd.fdf fdf;application/vnd.fdsn.mseed mseed;application/vnd.fdsn.seed dataless seed;\
    application/vnd.flographit gph;application/vnd.fluxtime.clip ftc;\
    application/vnd.framemaker book fm frame maker;application/vnd.frogans.fnc fnc;\
    application/vnd.frogans.ltf ltf;application/vnd.fsc.weblaunch fsc;application/vnd.fujitsu.oasys oas;\
    application/vnd.fujitsu.oasys2 oa2;application/vnd.fujitsu.oasys3 oa3;\
    application/vnd.fujitsu.oasysgp fg5;application/vnd.fujitsu.oasysprs bh2;\
    application/vnd.fujixerox.ddd ddd;application/vnd.fujixerox.docuworks xdw;\
    application/vnd.fujixerox.docuworks.binder xbd;application/vnd.fuzzysheet fzs;\
    application/vnd.genomatix.tuxedo txd;application/vnd.geogebra.file ggb;\
    application/vnd.geogebra.tool ggt;application/vnd.geometry-explorer gex gre;application/vnd.geonext gxt;\
    application/vnd.geoplan g2w;application/vnd.geospace g3w;application/vnd.gmx gmx;\
    application/vnd.google-apps.document gdoc;application/vnd.google-apps.presentation gslides;\
    application/vnd.google-apps.spreadsheet gsheet;application/vnd.google-earth.kml+xml kml;\
    application/vnd.google-earth.kmz kmz;application/vnd.grafeq gqf gqs;application/vnd.groove-account gac;\
    application/vnd.groove-help ghf;application/vnd.groove-identity-message gim;\
    application/vnd.groove-injector grv;application/vnd.groove-tool-message gtm;\
    application/vnd.groove-tool-template tpl;application/vnd.groove-vcard vcg;application/vnd.hal+xml hal;\
    application/vnd.handheld-entertainment+xml zmm;application/vnd.hbci hbci;\
    application/vnd.hhe.lesson-player les;application/vnd.hp-hpgl hpgl;application/vnd.hp-hpid hpid;\
    application/vnd.hp-hps hps;application/vnd.hp-jlyt jlt;application/vnd.hp-pcl pcl;\
    application/vnd.hp-pclxl pclxl;application/vnd.ibm.minipay mpy;\
    application/vnd.ibm.modcap afp list3820 listafp;application/vnd.ibm.rights-management irm;\
    application/vnd.ibm.secure-container sc;application/vnd.iccprofile icc icm;application/vnd.igloader igl;\
    application/vnd.immervision-ivp ivp;application/vnd.immervision-ivu ivu;application/vnd.insors.igm igm;\
    application/vnd.intercon.formnet xpw xpx;application/vnd.intergeo i2g;application/vnd.intu.qbo qbo;\
    application/vnd.intu.qfx qfx;application/vnd.ipunplugged.rcprofile rcprofile;\
    application/vnd.irepository.package+xml irp;application/vnd.is-xpr xpr;application/vnd.isac.fcs fcs;\
    application/vnd.jam jam;application/vnd.jcp.javame.midlet-rms rms;application/vnd.jisp jisp;\
    application/vnd.joost.joda-archive joda;application/vnd.kahootz ktr ktz;\
    application/vnd.kde.karbon karbon;application/vnd.kde.kchart chrt;application/vnd.kde.kformula kfo;\
    application/vnd.kde.kivio flw;application/vnd.kde.kontour kon;application/vnd.kde.kpresenter kpr kpt;\
    application/vnd.kde.kspread ksp;application/vnd.kde.kword kwd kwt;application/vnd.kenameaapp htke;\
    application/vnd.kidspiration kia;application/vnd.kinar kne knp;application/vnd.koan skd skm skp skt;\
    application/vnd.kodak-descriptor sse;application/vnd.las.las+xml lasxml;\
    application/vnd.llamagraphics.life-balance.desktop lbd;\
    application/vnd.llamagraphics.life-balance.exchange+xml lbe;application/vnd.lotus-1-2-3 123;\
    application/vnd.lotus-approach apr;application/vnd.lotus-freelance pre;application/vnd.lotus-notes nsf;\
    application/vnd.lotus-screencam scm;application/vnd.lotus-wordpro lwp;\
    application/vnd.macports.portpkg portpkg;application/vnd.mapbox-vector-tile mvt;application/vnd.mcd mcd;\
    application/vnd.medcalcdata mc1;application/vnd.mediastation.cdkey cdkey;application/vnd.mfer mwf;\
    application/vnd.mfmp mfm;application/vnd.micrografx.flo flo;application/vnd.micrografx.igx igx;\
    application/vnd.mif mif;application/vnd.mobius.daf daf;application/vnd.mobius.dis dis;\
    application/vnd.mobius.mbk mbk;application/vnd.mobius.mqy mqy;application/vnd.mobius.msl msl;\
    application/vnd.mobius.plc plc;application/vnd.mobius.txf txf;application/vnd.mophun.application mpn;\
    application/vnd.mophun.certificate mpc;application/vnd.mozilla.xul+xml xul;\
    application/vnd.ms-artgalry cil;application/vnd.ms-cab-compressed cab;\
    application/vnd.ms-excel xla xlc xlm xls xlt xlw;application/vnd.ms-excel.addin.macroenabled.12 xlam;\
    application/vnd.ms-excel.sheet.binary.macroenabled.12 xlsb;\
    application/vnd.ms-excel.sheet.macroenabled.12 xlsm;\
    application/vnd.ms-excel.template.macroenabled.12 xltm;application/vnd.ms-fontobject eot;\
    application/vnd.ms-htmlhelp chm;application/vnd.ms-ims ims;application/vnd.ms-lrm lrm;\
    application/vnd.ms-officetheme thmx;application/vnd.ms-outlook msg;application/vnd.ms-pki.seccat cat;\
    application/vnd.ms-powerpoint pot pps ppt;application/vnd.ms-powerpoint.addin.macroenabled.12 ppam;\
    application/vnd.ms-powerpoint.presentation.macroenabled.12 pptm;\
    application/vnd.ms-powerpoint.slide.macroenabled.12 sldm;\
    application/vnd.ms-powerpoint.slideshow.macroenabled.12 ppsm;\
    application/vnd.ms-powerpoint.template.macroenabled.12 potm;application/vnd.ms-project mpp mpt;\
    application/vnd.ms-word.document.macroenabled.12 docm;\
    application/vnd.ms-word.template.macroenabled.12 dotm;application/vnd.ms-works wcm wdb wks wps;\
    application/vnd.ms-wpl wpl;application/vnd.ms-xpsdocument xps;application/vnd.mseq mseq;\
    application/vnd.musician mus;application/vnd.muvee.style msty;application/vnd.mynfc taglet;\
    application/vnd.neurolanguage.nlu nlu;application/vnd.nitf nitf ntf;\
    application/vnd.noblenet-directory nnd;application/vnd.noblenet-sealer nns;\
    application/vnd.noblenet-web nnw;application/vnd.nokia.n-gage.ac+xml ac;\
    application/vnd.nokia.n-gage.data ngdat;application/vnd.nokia.radio-preset rpst;\
    application/vnd.nokia.radio-presets rpss;application/vnd.novadigm.edm edm;\
    application/vnd.novadigm.edx edx;application/vnd.novadigm.ext ext;\
    application/vnd.oasis.opendocument.chart odc;application/vnd.oasis.opendocument.chart-template otc;\
    application/vnd.oasis.opendocument.database odb;application/vnd.oasis.opendocument.formula odf;\
    application/vnd.oasis.opendocument.formula-template odft;\
    application/vnd.oasis.opendocument.graphics odg;\
    application/vnd.oasis.opendocument.graphics-template otg;application/vnd.oasis.opendocument.image odi;\
    application/vnd.oasis.opendocument.image-template oti;\
    application/vnd.oasis.opendocument.presentation odp;\
    application/vnd.oasis.opendocument.presentation-template otp;\
    application/vnd.oasis.opendocument.spreadsheet ods;\
    application/vnd.oasis.opendocument.spreadsheet-template ots;application/vnd.oasis.opendocument.text odt;\
    application/vnd.oasis.opendocument.text-master odm;application/vnd.oasis.opendocument.text-template ott;\
    application/vnd.oasis.opendocument.text-web oth;application/vnd.olpc-sugar xo;\
    application/vnd.oma.dd2+xml dd2;application/vnd.openblox.game+xml obgx;\
    application/vnd.openofficeorg.extension oxt;application/vnd.openstreetmap.data+xml osm;\
    application/vnd.openxmlformats-officedocument.presentationml.presentation pptx;\
    application/vnd.openxmlformats-officedocument.presentationml.slide sldx;\
    application/vnd.openxmlformats-officedocument.presentationml.slideshow ppsx;\
    application/vnd.openxmlformats-officedocument.presentationml.template potx;\
    application/vnd.openxmlformats-officedocument.spreadsheetml.sheet xlsx;\
    application/vnd.openxmlformats-officedocument.spreadsheetml.template xltx;\
    application/vnd.openxmlformats-officedocument.wordprocessingml.document docx;\
    application/vnd.openxmlformats-officedocument.wordprocessingml.template dotx;\
    application/vnd.osgeo.mapguide.package mgp;application/vnd.osgi.dp dp;\
    application/vnd.osgi.subsystem esa;application/vnd.palm oprc pqa;application/vnd.pawaafile paw;\
    application/vnd.pg.format str;application/vnd.pg.osasli ei6;application/vnd.picsel efif;\
    application/vnd.pmi.widget wg;application/vnd.pocketlearn plf;application/vnd.powerbuilder6 pbd;\
    application/vnd.previewsystems.box box;application/vnd.proteus.magazine mgz;\
    application/vnd.publishare-delta-tree qps;application/vnd.pvi.ptid1 ptid;\
    application/vnd.quark.quarkxpress qwd qwt qxb qxd qxl qxt;application/vnd.realvnc.bed bed;\
    application/vnd.recordare.musicxml mxl;application/vnd.recordare.musicxml+xml musicxml;\
    application/vnd.rig.cryptonote cryptonote;application/vnd.rim.cod cod;application/vnd.rn-realmedia rm;\
    application/vnd.rn-realmedia-vbr rmvb;application/vnd.route66.link66+xml link66;\
    application/vnd.sailingtracker.track st;application/vnd.seemail see;application/vnd.sema sema;\
    application/vnd.semd semd;application/vnd.semf semf;application/vnd.shana.informed.formdata ifm;\
    application/vnd.shana.informed.formtemplate itp;application/vnd.shana.informed.interchange iif;\
    application/vnd.shana.informed.package ipk;application/vnd.simtech-mindmapper twd twds;\
    application/vnd.smaf mmf;application/vnd.smart.teacher teacher;\
    application/vnd.software602.filler.form+xml fo;application/vnd.solent.sdkm+xml sdkd sdkm;\
    application/vnd.spotfire.dxp dxp;application/vnd.spotfire.sfs sfs;application/vnd.stardivision.calc sdc;\
    application/vnd.stardivision.draw sda;application/vnd.stardivision.impress sdd;\
    application/vnd.stardivision.math smf;application/vnd.stardivision.writer sdw vor;\
    application/vnd.stardivision.writer-global sgl;application/vnd.stepmania.package smzip;\
    application/vnd.stepmania.stepchart sm;application/vnd.sun.wadl+xml wadl;\
    application/vnd.sun.xml.calc sxc;application/vnd.sun.xml.calc.template stc;\
    application/vnd.sun.xml.draw sxd;application/vnd.sun.xml.draw.template std;\
    application/vnd.sun.xml.impress sxi;application/vnd.sun.xml.impress.template sti;\
    application/vnd.sun.xml.math sxm;application/vnd.sun.xml.writer sxw;\
    application/vnd.sun.xml.writer.global sxg;application/vnd.sun.xml.writer.template stw;\
    application/vnd.sus-calendar sus susp;application/vnd.svd svd;application/vnd.symbian.install sis sisx;\
    application/vnd.syncml+xml xsm;application/vnd.syncml.dm+wbxml bdm;application/vnd.syncml.dm+xml xdm;\
    application/vnd.syncml.dmddf+xml ddf;application/vnd.tao.intent-module-archive tao;\
    application/vnd.tcpdump.pcap cap dmp pcap;application/vnd.tmobile-livetv tmo;\
    application/vnd.trid.tpt tpt;application/vnd.triscape.mxs mxs;application/vnd.trueapp tra;\
    application/vnd.ufdl ufd ufdl;application/vnd.uiq.theme utz;application/vnd.umajin umj;\
    application/vnd.unity unityweb;application/vnd.uoml+xml uoml;application/vnd.vcx vcx;\
    application/vnd.visio vsd vss vst vsw;application/vnd.visionary vis;application/vnd.vsf vsf;\
    application/vnd.wap.wbxml wbxml;application/vnd.wap.wmlc wmlc;application/vnd.wap.wmlscriptc wmlsc;\
    application/vnd.webturbo wtb;application/vnd.wolfram.player nbp;application/vnd.wordperfect wpd;\
    application/vnd.wqd wqd;application/vnd.wt.stf stf;application/vnd.xara xar;application/vnd.xfdl xfdl;\
    application/vnd.yamaha.hv-dic hvd;application/vnd.yamaha.hv-script hvs;\
    application/vnd.yamaha.hv-voice hvp;application/vnd.yamaha.openscoreformat osf;\
    application/vnd.yamaha.openscoreformat.osfpvg+xml osfpvg;application/vnd.yamaha.smaf-audio saf;\
    application/vnd.yamaha.smaf-phrase spf;application/vnd.yellowriver-custom-menu cmp;\
    application/vnd.zul zir zirz;application/vnd.zzazz.deck+xml zaz;application/voicexml+xml vxml;\
    application/wasm wasm;application/watcherinfo+xml wif;application/widget wgt;application/winhlp hlp;\
    application/wsdl+xml wsdl;application/wspolicy+xml wspolicy;application/x-7z-compressed 7z;\
    application/x-abiword abw;application/x-ace-compressed ace;application/x-apple-diskimage dmg;\
    application/x-arj arj;application/x-authorware-bin aab u32 vox x32;application/x-authorware-map aam;\
    application/x-authorware-seg aas;application/x-bcpio bcpio;application/x-bdoc bdoc;\
    application/x-bittorrent torrent;application/x-blorb blb blorb;application/x-bzip bz;\
    application/x-bzip2 boz bz2;application/x-cbr cb7 cba cbr cbt cbz;application/x-cdlink vcd;\
    application/x-cfs-compressed cfs;application/x-chat chat;application/x-chess-pgn pgn;\
    application/x-chrome-extension crx;application/x-cocoa cco;application/x-conference nsc;\
    application/x-cpio cpio;application/x-csh csh;application/x-debian-package deb udeb;\
    application/x-dgc-compressed dgc;application/x-director cct cst cxt dcr dir dxr fgd swa w3d;\
    application/x-doom wad;application/x-dtbncx+xml ncx;application/x-dtbook+xml dtb;\
    application/x-dtbresource+xml res;application/x-dvi dvi;application/x-envoy evy;application/x-eva eva;\
    application/x-font-bdf bdf;application/x-font-ghostscript gsf;application/x-font-linux-psf psf;\
    application/x-font-pcf pcf;application/x-font-snf snf;application/x-font-type1 afm pfa pfb pfm;\
    application/x-freearc arc;application/x-futuresplash spl;application/x-gca-compressed gca;\
    application/x-glulx ulx;application/x-gnumeric gnumeric;application/x-gramps-xml gramps;\
    application/x-gtar gtar;application/x-hdf hdf;application/x-httpd-php php;\
    application/x-install-instructions install;application/x-iso9660-image iso;\
    application/x-iwork-keynote-sffkey key;application/x-iwork-numbers-sffnumbers numbers;\
    application/x-iwork-pages-sffpages pages;application/x-java-archive-diff jardiff;\
    application/x-java-jnlp-file jnlp;application/x-keepass2 kdbx;application/x-latex latex;\
    application/x-lua-bytecode luac;application/x-lzh-compressed lha lzh;application/x-makeself run;\
    application/x-mie mie;application/x-mobipocket-ebook mobi;application/x-ms-application application;\
    application/x-ms-shortcut lnk;application/x-ms-wmd wmd;application/x-ms-xbap xbap;\
    application/x-msaccess mdb;application/x-msbinder obd;application/x-mscardfile crd;\
    application/x-msclip clp;application/x-msdownload bat com dll exe msi;\
    application/x-msmediaview m13 m14 mvb;application/x-msmetafile emz wmz;application/x-msmoney mny;\
    application/x-mspublisher pub;application/x-msschedule scd;application/x-msterminal trm;\
    application/x-mswrite wri;application/x-netcdf cdf nc;application/x-ns-proxy-autoconfig pac;\
    application/x-nzb nzb;application/x-perl pl pm;application/x-pilot pdb;application/x-pkcs12 p12 pfx;\
    application/x-pkcs7-certificates p7b spc;application/x-pkcs7-certreqresp p7r;\
    application/x-rar-compressed rar;application/x-redhat-package-manager rpm;\
    application/x-research-info-systems ris;application/x-sea sea;application/x-sh sh;\
    application/x-shar shar;application/x-shockwave-flash swf;application/x-silverlight-app xap;\
    application/x-sql sql;application/x-stuffit sit;application/x-stuffitx sitx;application/x-subrip srt;\
    application/x-sv4cpio sv4cpio;application/x-sv4crc sv4crc;application/x-t3vm-image t3;\
    application/x-tads gam;application/x-tar tar;application/x-tcl tcl tk;application/x-tex tex;\
    application/x-tex-tfm tfm;application/x-texinfo texi texinfo;application/x-ustar ustar;\
    application/x-virtualbox-hdd hdd;application/x-virtualbox-ova ova;application/x-virtualbox-ovf ovf;\
    application/x-virtualbox-vbox vbox;application/x-virtualbox-vdi vdi;application/x-virtualbox-vhd vhd;\
    application/x-virtualbox-vmdk vmdk;application/x-wais-source src;\
    application/x-web-app-manifest+json webapp;application/x-x509-ca-cert crt der pem;\
    application/x-xfig fig;application/x-xpinstall xpi;application/x-xz xz;\
    application/x-zmachine z1 z2 z3 z4 z5 z6 z7 z8;application/xaml+xml xaml;application/xcap-att+xml xav;\
    application/xcap-caps+xml xca;application/xcap-diff+xml xdf;application/xcap-el+xml xel;\
    application/xcap-ns+xml xns;application/xenc+xml xenc;application/xhtml+xml xht xhtml;\
    application/xliff+xml xlf;application/xml rng xml xsd;application/xml-dtd dtd;application/xop+xml xop;\
    application/xproc+xml xpl;application/xslt+xml xsl xslt;application/xspf+xml xspf;\
    application/xv+xml mxml xhvml xvm xvml;application/yang yang;application/yin+xml yin;\
    application/zip zip;audio/adpcm adp;audio/amr amr;audio/basic au snd;audio/midi kar mid midi rmi;\
    audio/mobile-xmf mxmf;audio/mp4 mp4a;audio/mpeg m2a m3a mp2 mp2a mp3 mpga;audio/ogg oga ogg opus spx;\
    audio/s3m s3m;audio/silk sil;audio/vnd.dece.audio uva uvva;audio/vnd.digital-winds eol;\
    audio/vnd.dra dra;audio/vnd.dts dts;audio/vnd.dts.hd dtshd;audio/vnd.lucent.voice lvp;\
    audio/vnd.ms-playready.media.pya pya;audio/vnd.nuera.ecelp4800 ecelp4800;\
    audio/vnd.nuera.ecelp7470 ecelp7470;audio/vnd.nuera.ecelp9600 ecelp9600;audio/vnd.rip rip;\
    audio/webm weba;audio/x-aac aac;audio/x-aiff aif aifc aiff;audio/x-caf caf;audio/x-flac flac;\
    audio/x-m4a m4a;audio/x-matroska mka;audio/x-mpegurl m3u;audio/x-ms-wax wax;audio/x-ms-wma wma;\
    audio/x-pn-realaudio ram;audio/x-pn-realaudio-plugin rmp;audio/x-realaudio ra;audio/x-wav wav;\
    audio/xm xm;chemical/x-cdx cdx;chemical/x-cif cif;chemical/x-cmdf cmdf;chemical/x-cml cml;\
    chemical/x-csml csml;chemical/x-xyz xyz;font/collection ttc;font/otf otf;font/ttf ttf;font/woff woff;\
    font/woff2 woff2;image/aces exr;image/apng apng;image/avci avci;image/avcs avcs;image/avif avif;\
    image/cgm cgm;image/dicom-rle drle;image/emf emf;image/fits fits;image/g3fax g3;image/gif gif;\
    image/heic heic;image/heic-sequence heics;image/heif heif;image/heif-sequence heifs;image/hej2k hej2;\
    image/hsj2 hsj2;image/ief ief;image/jls jls;image/jp2 jp2 jpg2;image/jpeg jpe jpeg jpg;image/jph jph;\
    image/jphc jhc;image/jpx jpf jpx;image/jxr jxr;image/jxra jxra;image/jxrs jxrs;image/jxs jxs;\
    image/jxsc jxsc;image/jxsi jxsi;image/jxss jxss;image/ktx ktx;image/ktx2 ktx2;image/png png;\
    image/prs.btif btif;image/prs.pti pti;image/sgi sgi;image/svg+xml svg svgz;image/t38 t38;\
    image/tiff tif tiff;image/tiff-fx tfx;image/vnd.adobe.photoshop psd;\
    image/vnd.airzip.accelerator.azv azv;image/vnd.dece.graphic uvg uvi uvvg uvvi;image/vnd.djvu djv djvu;\
    image/vnd.dwg dwg;image/vnd.dxf dxf;image/vnd.fastbidsheet fbs;image/vnd.fpx fpx;image/vnd.fst fst;\
    image/vnd.fujixerox.edmics-mmr mmr;image/vnd.fujixerox.edmics-rlc rlc;image/vnd.ms-dds dds;\
    image/vnd.ms-modi mdi;image/vnd.ms-photo wdp;image/vnd.net-fpx npx;image/vnd.pco.b16 b16;\
    image/vnd.tencent.tap tap;image/vnd.valve.source.texture vtf;image/vnd.wap.wbmp wbmp;image/vnd.xiff xif;\
    image/webp webp;image/wmf wmf;image/x-3ds 3ds;image/x-cmu-raster ras;image/x-cmx cmx;\
    image/x-freehand fh fh4 fh5 fh7 fhc;image/x-icon ico;image/x-jng jng;image/x-mrsid-image sid;\
    image/x-ms-bmp bmp;image/x-pcx pcx;image/x-pict pct pic;image/x-portable-anymap pnm;\
    image/x-portable-bitmap pbm;image/x-portable-graymap pgm;image/x-portable-pixmap ppm;image/x-rgb rgb;\
    image/x-tga tga;image/x-xbitmap xbm;image/x-xpixmap xpm;image/x-xwindowdump xwd;message/global u8msg;\
    message/global-delivery-status u8dsn;message/global-disposition-notification u8mdn;\
    message/global-headers u8hdr;message/rfc822 eml mime;message/vnd.wfa.wsc wsc;model/3mf 3mf;\
    model/gltf+json gltf;model/gltf-binary glb;model/iges iges igs;model/mesh mesh msh silo;model/mtl mtl;\
    model/obj obj;model/prc prc;model/step+xml stpx;model/step+zip stpz;model/step-xml+zip stpxz;\
    model/stl stl;model/u3d u3d;model/vnd.collada+xml dae;model/vnd.dwf dwf;model/vnd.gdl gdl;\
    model/vnd.gtw gtw;model/vnd.opengex ogex;model/vnd.parasolid.transmit.binary x_b;\
    model/vnd.parasolid.transmit.text x_t;model/vnd.sap.vds vds;model/vnd.usdz+zip usdz;\
    model/vnd.valve.source.compiled-map bsp;model/vnd.vtu vtu;model/vrml vrml wrl;model/x3d+binary x3dbz;\
    model/x3d+fastinfoset x3db;model/x3d+vrml x3dv x3dvz;model/x3d+xml x3d x3dz;\
    text/cache-manifest appcache manifest;text/calendar ics ifb;text/coffeescript coffee litcoffee;\
    text/css css;text/csv csv;text/html htm html shtml;text/jade jade;\
    text/javascript cjs cts js jsx mjs mts mtsx ts tsx;text/less less;text/markdown markdown md;\
    text/mathml mml;text/mdx mdx;text/n3 n3;text/plain conf def in ini list log text txt;\
    text/prs.lines.tag dsc;text/richtext rtx;text/rtf rtf;text/sgml sgm sgml;text/shex shex;\
    text/slim slim slm;text/spdx spdx;text/stylus styl stylus;text/tab-separated-values tsv;\
    text/troff man me ms roff t tr;text/turtle ttl;text/uri-list uri uris urls;text/vcard vcard;\
    text/vnd.curl curl;text/vnd.curl.dcurl dcurl;text/vnd.curl.mcurl mcurl;text/vnd.curl.scurl scurl;\
    text/vnd.dvb.subtitle sub;text/vnd.familysearch.gedcom ged;text/vnd.fly fly;text/vnd.fmi.flexstor flx;\
    text/vnd.graphviz gv;text/vnd.in3d.3dml 3dml;text/vnd.in3d.spot spot;\
    text/vnd.sun.j2me.app-descriptor jad;text/vnd.wap.wml wml;text/vnd.wap.wmlscript wmls;text/vtt vtt;\
    text/x-asm asm s;text/x-c c cc cpp cxx dic h hh;text/x-component htc;text/x-fortran f f77 f90 for;\
    text/x-handlebars-template hbs;text/x-java-source java;text/x-lua lua;text/x-markdown mkd;\
    text/x-nfo nfo;text/x-opml opml;text/x-org org;text/x-pascal p pas;text/x-processing pde;\
    text/x-sass sass;text/x-scss scss;text/x-setext etx;text/x-sfv sfv;text/x-suse-ymp ymp;\
    text/x-uuencode uu;text/x-vcalendar vcs;text/x-vcard vcf;text/yaml yaml yml;video/3gpp 3gp 3gpp;\
    video/3gpp2 3g2;video/h261 h261;video/h263 h263;video/h264 h264;video/iso.segment m4s;video/jpeg jpgv;\
    video/jpm jpgm jpm;video/mj2 mj2 mjp2;video/mp4 mp4 mp4v mpg4;video/mpeg m1v m2v mpe mpeg mpg;\
    video/ogg ogv;video/quicktime mov qt;video/vnd.dece.hd uvh uvvh;video/vnd.dece.mobile uvm uvvm;\
    video/vnd.dece.pd uvp uvvp;video/vnd.dece.sd uvs uvvs;video/vnd.dece.video uvv uvvv;\
    video/vnd.dvb.file dvb;video/vnd.fvt fvt;video/vnd.mpegurl m4u mxu;video/vnd.ms-playready.media.pyv pyv;\
    video/vnd.uvvu.mp4 uvu uvvu;video/vnd.vivo viv;video/webm webm;video/x-f4v f4v;video/x-fli fli;\
    video/x-flv flv;video/x-m4v m4v;video/x-matroska mk3d mks mkv;video/x-mng mng;video/x-ms-asf asf asx;\
    video/x-ms-vob vob;video/x-ms-wm wm;video/x-ms-wmv wmv;video/x-ms-wmx wmx;video/x-ms-wvx wvx;\
    video/x-msvideo avi;video/x-sgi-movie movie;video/x-smv smv;x-conference/x-cooltalk ice;\
    ";

static MIME_TYPES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut types = HashMap::new();
    for entry in MIME_TYPE_TABLE.split(';') {
        let mut words = entry.split(' ');
        let Some(mime_type) = words.next().filter(|word| !word.is_empty()) else {
            continue;
        };
        types.extend(words.map(|extension| (extension, mime_type)));
    }
    types
});

/// A string in double quotes the way Bun's error messages show one.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c < ' ' || matches!(c, '\u{2028}' | '\u{2029}') => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Longest path, in bytes, Bun takes a file name for.
const MAX_PATH_BYTES: usize = 4096;

/// The MIME type of a file by its extension, as `Bun.file(name).type` knows
/// it; `application/octet-stream` for an unknown one. A file name Bun
/// refuses to make a file of (a NUL in its extension, or an extension as
/// long as a path may be) is an error here too.
pub fn mime_type_from_filename(filename: &str) -> Result<String, MimeParseError> {
    let lower: String = filename
        .to_lowercase()
        .chars()
        .filter(|c| !jsstr::is_space(*c))
        .collect();
    let extension = lower.rsplit('.').next().unwrap_or("");
    if extension.is_empty() {
        return Ok("application/octet-stream".to_string());
    }
    let path = format!("x.{extension}");
    if path.contains('\0') {
        return Err(MimeParseError::FilenameWithNul(quoted(&path)));
    }
    if path.len() >= MAX_PATH_BYTES {
        let message = format!("ENAMETOOLONG: name too long, open '{path}'");
        let shown = String::from_utf8_lossy(&message.as_bytes()[..MAX_PATH_BYTES]).into_owned();
        return Err(MimeParseError::FilenameTooLong(shown));
    }
    // The extension is what follows the last dot of the last path segment.
    let extension = extension.trim_end_matches('/');
    let known = if extension.contains('/') {
        None
    } else {
        MIME_TYPES.get(extension)
    };
    Ok(known.copied().unwrap_or("application/octet-stream").to_string())
}

/// `path.parse(path).ext`: the last path segment from its last dot on, when
/// something other than dots comes before that dot.
fn path_extension(path: &str) -> &str {
    let bytes = path.as_bytes();
    let start = usize::from(bytes.first() == Some(&b'/'));
    let mut start_dot = None;
    let mut start_part = 0;
    let mut end = None;
    // Characters seen before the first dot: none yet (0), only dots (1), others (-1).
    let mut pre_dot_state = 0;
    for i in (start..bytes.len()).rev() {
        let code = bytes[i];
        if code == b'/' {
            // A separator ends the segment, unless it is one of those that end the path.
            if end.is_some() {
                start_part = i + 1;
                break;
            }
            continue;
        }
        end.get_or_insert(i + 1);
        if code == b'.' {
            if start_dot.is_none() {
                start_dot = Some(i);
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot.is_some() {
            pre_dot_state = -1;
        }
    }
    match (start_dot, end) {
        (Some(start_dot), Some(end)) => {
            // Nothing before the dot, or the segment is exactly "..".
            let only_dots = pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1;
            if pre_dot_state == 0 || only_dots {
                ""
            } else {
                &path[start_dot..end]
            }
        }
        _ => "",
    }
}

type PartId = usize;

#[derive(Default)]
struct Part {
    parent: Option<PartId>,
    children: Vec<PartId>,
    header_bytes: Vec<u8>,
    header_lines: Option<Vec<RawHeader>>,
    content_type: Option<String>,
    charset: Option<String>,
    encoding: String,
    disposition: Option<String>,
    filename: Option<String>,
    boundary: Option<Vec<u8>>,
    parent_boundary: Option<Vec<u8>>,
    multipart: bool,
    flowed: bool,
    del_sp: bool,
    is_rfc822: bool,
    message_node: bool,
    meta_headers: Option<EmbeddedHeaders>,
    body: Option<PartBody>,
    text_content: Option<String>,
}

fn read_part_structure(part: &mut Part) -> Result<(), MimeParseError> {
    let lines = split_header_block(&std::mem::take(&mut part.header_bytes));

    let disposition = parse_header_value(&first_header(&lines, "content-disposition"));
    let mut content_type_header = first_header(&lines, "content-type");
    if !lines.iter().any(|line| line.key == "content-type") {
        let extension = path_extension(disposition.params.get("filename").unwrap_or(""));
        let extension = extension.strip_prefix('.').unwrap_or(extension);
        content_type_header = if !extension.is_empty() {
            mime_type_from_filename(extension)?
        } else if disposition.value.eq_ignore_ascii_case("attachment") {
            "application/octet-stream".to_string()
        } else {
            "text/plain".to_string()
        };
    }
    let content_type = parse_header_value(&content_type_header);
    let param = |name: &str| content_type.params.get(name).filter(|value| !value.is_empty());

    part.encoding =
        jsstr::trim(&without_comment(&first_header(&lines, "content-transfer-encoding")).to_lowercase()).to_string();
    part.content_type =
        Some(jsstr::trim(&content_type.value.to_lowercase()).to_string()).filter(|value| !value.is_empty());
    part.charset = param("charset").map(str::to_string);
    let disposition_value = disposition.value.to_lowercase();
    let disposition_value = jsstr::trim(&disposition_value);
    part.disposition = (!disposition_value.is_empty()).then(|| decode_words(disposition_value));

    let raw_filename = disposition
        .params
        .get("filename")
        .filter(|value| !value.is_empty())
        .or(param("name"));
    part.filename = raw_filename.map(|raw| decode_words(&raw_octets_as_utf8(raw)));

    let is =
        |value: Option<&str>, expected: &str| value.is_some_and(|value| jsstr::trim(&value.to_lowercase()) == expected);
    if is(content_type.params.get("format"), "flowed") {
        part.flowed = true;
        part.del_sp = is(content_type.params.get("delsp"), "yes");
    }

    part.multipart = part
        .content_type
        .as_deref()
        .is_some_and(|value| value.starts_with("multipart/"));
    part.boundary = param("boundary").map(latin1_bytes);
    part.is_rfc822 = part.content_type.as_deref() == Some("message/rfc822");
    part.header_lines = Some(lines);
    Ok(())
}

/// Bytes staged before an attachment file write is issued.
const WRITE_BATCH_BYTES: usize = 256 * 1024;

/// What the parser asks of the file system. It decides them while reading a
/// chunk; they are carried out before the next one is read.
enum FileOp {
    Create(usize, PathBuf),
    Write(usize, Vec<u8>),
    Close(usize),
}

/// An attachment file being written.
struct FileBody {
    index: usize,
    staged: Vec<u8>,
    size: u64,
}

impl FileBody {
    fn write(&mut self, bytes: &[u8], ops: &mut Vec<FileOp>) {
        self.staged.extend_from_slice(bytes);
        self.size += bytes.len() as u64;
        if self.staged.len() >= WRITE_BATCH_BYTES {
            ops.push(FileOp::Write(self.index, std::mem::take(&mut self.staged)));
        }
    }

    fn finish(&mut self, ops: &mut Vec<FileOp>) {
        if !self.staged.is_empty() {
            ops.push(FileOp::Write(self.index, std::mem::take(&mut self.staged)));
        }
        ops.push(FileOp::Close(self.index));
    }
}

/// Where a leaf part's decoded body goes.
struct PartBody {
    decoder: TransferDecoder,
    sink: BodySink,
}

enum BodySink {
    /// A text body held in memory.
    Text(Vec<u8>),
    File(FileBody),
}

/// Undoes RFC 3676 format=flowed soft breaks and space-stuffing.
fn decode_flowed(text: &[u8], del_sp: bool) -> Vec<u8> {
    let mut result: Vec<Vec<u8>> = Vec::new();
    // The line being joined, `None` before the first one.
    let mut joined: Option<Vec<u8>> = None;
    for line in split_lines(text) {
        match joined.as_mut() {
            // RFC 3676 §4.3: the "-- " signature separator is never a soft break.
            Some(current) if current.ends_with(b" ") && current != b"-- " => {
                if del_sp {
                    current.pop();
                }
                current.extend_from_slice(line);
            }
            _ => {
                result.extend(joined.replace(line.to_vec()));
            }
        }
    }
    result.extend(joined.filter(|last| !last.is_empty()));
    let text = result.join(&b'\n');
    // A space that starts a line is stuffing.
    let mut unstuffed = Vec::with_capacity(text.len());
    for (i, &byte) in text.iter().enumerate() {
        if byte == b' ' && (i == 0 || matches!(text[i - 1], b'\n' | b'\r')) {
            continue;
        }
        unstuffed.push(byte);
    }
    unstuffed
}

fn spill_filename(content_type: &str) -> &'static str {
    match content_type {
        "text/html" => "body.html",
        "text/plain" => "body.txt",
        _ => "body",
    }
}

fn trailing_break_length(bytes: &[u8]) -> usize {
    match bytes {
        [.., b'\r', b'\n'] => 2,
        [.., b'\n'] => 1,
        _ => 0,
    }
}

/// How a line stands to the boundaries around the current part.
#[derive(Clone, Copy, PartialEq)]
enum Boundary {
    None,
    /// The next child of the current multipart.
    NextChild,
    /// The end of the current multipart.
    OwnEnd,
    /// The next part of the enclosing multipart.
    NextSibling,
    /// The end of the enclosing multipart.
    ParentEnd,
}

/// 0 no match, 1 a delimiter, 2 a closing delimiter.
fn compare_boundary(line: &[u8], start: usize, boundary: &[u8]) -> u8 {
    if line.len() < boundary.len() + 3 + start || line.len() > boundary.len() + 6 + start {
        return 0;
    }
    if &line[start + 2..start + 2 + boundary.len()] != boundary {
        return 0;
    }
    for (pos, &c) in line[boundary.len() + 2 + start..].iter().enumerate() {
        let line_break = c == b'\r' || c == b'\n';
        match pos {
            0 if line_break => return 1,
            0 | 1 if c != b'-' => return 0,
            2 if !line_break => return 0,
            3 if c != b'\n' => return 0,
            _ => {}
        }
    }
    2
}

/// Streaming MIME parser. Bytes are split into parts as they arrive;
/// attachment parts are transfer-decoded straight into files under the
/// attachment directory, and only text bodies (within
/// [`TEXT_BODY_BUDGET_BYTES`]) and headers are held in memory.
struct MimeParser {
    attachment_dir: PathBuf,
    parts: Vec<Part>,
    /// The part being read.
    part: PartId,
    stack: Vec<PartId>,
    in_header: bool,
    part_count: usize,

    partial: Vec<u8>,
    continuing_line: bool,
    pending_break: Vec<u8>,

    message_headers: MessageHeaders,
    has_text: bool,
    has_html: bool,
    text_budget: usize,
    attachments: Vec<(PartId, ParsedAttachment)>,
    file_counter: usize,
    ops: Vec<FileOp>,
    scratch: Vec<u8>,
}

const ROOT: PartId = 0;

impl MimeParser {
    fn new(options: &ParseOptions) -> MimeParser {
        MimeParser {
            attachment_dir: options.attachment_dir.clone(),
            parts: vec![Part::default()],
            part: ROOT,
            stack: vec![ROOT],
            in_header: true,
            part_count: 1,
            partial: Vec::new(),
            continuing_line: false,
            pending_break: Vec::new(),
            message_headers: MessageHeaders::default(),
            has_text: false,
            has_html: false,
            text_budget: TEXT_BODY_BUDGET_BYTES,
            attachments: Vec::new(),
            file_counter: 0,
            ops: Vec::new(),
            scratch: Vec::new(),
        }
    }

    fn current(&self) -> &Part {
        &self.parts[self.part]
    }

    fn new_file(&mut self) -> (FileBody, PathBuf) {
        let index = self.file_counter;
        self.file_counter += 1;
        let path = self.attachment_dir.join(index.to_string());
        self.ops.push(FileOp::Create(index, path.clone()));
        (
            FileBody {
                index,
                staged: Vec::new(),
                size: 0,
            },
            path,
        )
    }

    /// Longest line that could still be a boundary delimiter of the current part.
    fn boundary_line_limit(&self) -> usize {
        let part = self.current();
        let own = part.boundary.as_ref().map_or(0, Vec::len);
        let parent = part.parent_boundary.as_ref().map_or(0, Vec::len);
        own.max(parent) + 8
    }

    fn write(&mut self, chunk: &[u8]) -> Result<(), MimeParseError> {
        if self.partial.is_empty() {
            return self.process_lines(chunk);
        }
        let mut joined = std::mem::take(&mut self.partial);
        match chunk.iter().position(|&byte| byte == b'\n') {
            None => {
                joined.extend_from_slice(chunk);
                self.process_lines(&joined)
            }
            // Only the carried-over line is joined; the rest of the chunk is read in place.
            Some(lf) => {
                joined.extend_from_slice(&chunk[..=lf]);
                self.process_lines(&joined)?;
                self.process_lines(&chunk[lf + 1..])
            }
        }
    }

    fn process_lines(&mut self, data: &[u8]) -> Result<(), MimeParseError> {
        self.partial.clear();
        let mut line_start = 0;
        // Consecutive body lines are handed on as one run.
        let mut group: Option<(usize, usize)> = None;

        while let Some(found) = data[line_start..].iter().position(|&byte| byte == b'\n') {
            let line_end = line_start + found + 1;
            let line = &data[line_start..line_end];
            let continuation = std::mem::take(&mut self.continuing_line);

            let boundary = if continuation {
                Boundary::None
            } else {
                self.check_boundary(line)
            };
            if boundary == Boundary::None && !self.in_header && !self.current().multipart {
                group = Some((group.map_or(line_start, |(start, _)| start), line_end));
                line_start = line_end;
                continue;
            }
            if let Some((start, end)) = group.take() {
                self.body_bytes(&data[start..end], false)?;
            }
            if boundary != Boundary::None {
                self.on_boundary(boundary)?;
            } else if self.in_header {
                self.header_line(line, false)?;
            }
            line_start = line_end;
        }

        let mut rest = &data[line_start..];
        let cannot_be_boundary = self.continuing_line || rest.len() > self.boundary_line_limit();
        if !rest.is_empty() && !self.in_header && cannot_be_boundary {
            // A trailing CR may be the first half of this line's CRLF, so it waits.
            let held = usize::from(rest.last() == Some(&b'\r'));
            if !self.current().multipart && rest.len() > held {
                group = Some((group.map_or(line_start, |(start, _)| start), data.len() - held));
            }
            self.continuing_line = true;
            rest = &rest[rest.len() - held..];
        }
        if let Some((start, end)) = group.take() {
            self.body_bytes(&data[start..end], false)?;
        }
        if !rest.is_empty() {
            if self.in_header && self.current().header_bytes.len() + rest.len() > MAX_PART_HEADER_BYTES {
                return Err(MimeParseError::HeaderTooLarge);
            }
            self.partial = rest.to_vec();
        }
        Ok(())
    }

    fn end(&mut self) -> Result<ParsedMessage, MimeParseError> {
        let rest = std::mem::take(&mut self.partial);
        let boundary = if self.continuing_line {
            Boundary::None
        } else {
            self.check_boundary(&rest)
        };
        if boundary != Boundary::None {
            self.on_boundary(boundary)?;
        } else if self.in_header {
            self.header_line(&rest, true)?;
        } else if !self.current().multipart {
            self.body_bytes(&rest, true)?;
        }
        if !self.in_header && !self.current().multipart {
            self.body_bytes(&[], true)?;
        }
        self.finish_part(self.part);
        self.result()
    }

    fn check_boundary(&self, line: &[u8]) -> Boundary {
        let mut start = 0;
        if matches!(line.first(), Some(b'\r' | b'\n')) {
            start += 1;
            if line.len() >= 2 && (line[0] == b'\r' || line[1] == b'\n') {
                start += 1;
            }
        }
        if line.len() < 4 || line.get(start) != Some(&b'-') || line.get(start + 1) != Some(&b'-') {
            return Boundary::None;
        }
        let part = self.current();
        match part
            .boundary
            .as_ref()
            .map_or(0, |boundary| compare_boundary(line, start, boundary))
        {
            1 => return Boundary::NextChild,
            2 => return Boundary::OwnEnd,
            _ => {}
        }
        match part
            .parent_boundary
            .as_ref()
            .map_or(0, |boundary| compare_boundary(line, start, boundary))
        {
            1 => Boundary::NextSibling,
            2 => Boundary::ParentEnd,
            _ => Boundary::None,
        }
    }

    fn on_boundary(&mut self, kind: Boundary) -> Result<(), MimeParseError> {
        self.pending_break.clear();
        self.finish_part(self.part);
        match kind {
            Boundary::NextChild => self.start_part(Some(self.part))?,
            Boundary::NextSibling => {
                let mut parent = self.current().parent;
                if let Some(message) = parent.filter(|&parent| self.parts[parent].is_rfc822) {
                    parent = self.parts[message].parent;
                }
                self.start_part(parent)?;
            }
            Boundary::ParentEnd => {
                let part = self.current();
                if self.in_header && !part.header_bytes.is_empty() && part.header_lines.is_none() {
                    self.complete_header()?;
                }
                if let Some(enclosing) = self.stack.pop() {
                    self.part = enclosing;
                }
                self.in_header = false;
            }
            Boundary::OwnEnd | Boundary::None => {}
        }
        Ok(())
    }

    fn start_part(&mut self, parent: Option<PartId>) -> Result<(), MimeParseError> {
        self.part_count += 1;
        if self.part_count > MAX_PARTS {
            return Err(MimeParseError::TooManyParts);
        }
        let parent_boundary = parent.and_then(|parent| self.parts[parent].boundary.clone());
        self.parts.push(Part {
            parent,
            parent_boundary,
            ..Part::default()
        });
        self.part = self.parts.len() - 1;
        self.in_header = true;
        Ok(())
    }

    fn header_line(&mut self, line: &[u8], last: bool) -> Result<(), MimeParseError> {
        let part = &mut self.parts[self.part];
        part.header_bytes.extend_from_slice(line);
        if part.header_bytes.len() > MAX_PART_HEADER_BYTES {
            return Err(MimeParseError::HeaderTooLarge);
        }
        if last || line == b"\n" || line == b"\r\n" {
            self.complete_header()?;
        }
        Ok(())
    }

    fn complete_header(&mut self) -> Result<(), MimeParseError> {
        let id = self.part;
        read_part_structure(&mut self.parts[id])?;
        if let Some(parent) = self.parts[id].parent {
            self.parts[parent].children.push(id);
        }
        self.continuing_line = false;

        let part = &self.parts[id];
        let embeds_message = part.is_rfc822
            && matches!(part.encoding.as_str(), "" | "7bit" | "8bit" | "binary")
            && part.disposition.as_deref() == Some("inline");

        if embeds_message {
            let enclosing_boundary = part.parent.and_then(|parent| self.parts[parent].boundary.clone());
            self.parts[id].message_node = true;
            self.start_part(Some(id))?;
            self.parts[self.part].parent_boundary = enclosing_boundary;
        } else {
            self.in_header = false;
            if part.multipart && part.boundary.is_some() {
                self.stack.push(id);
            }
        }

        self.open_body(id)
    }

    fn open_body(&mut self, id: PartId) -> Result<(), MimeParseError> {
        let lines = self.parts[id].header_lines.take().unwrap_or_default();
        if id == ROOT {
            self.message_headers = read_message_headers(&lines);
        }
        if self.parts[id].parent.is_some_and(|parent| self.parts[parent].is_rfc822) {
            self.parts[id].meta_headers = Some(read_message_headers(&lines).embedded);
        }
        let opened = self.open_leaf_body(id, &lines);
        self.parts[id].header_lines = Some(lines);
        opened
    }

    /// Decides where the body of a part that is not a container goes.
    fn open_leaf_body(&mut self, id: PartId, lines: &[RawHeader]) -> Result<(), MimeParseError> {
        let part = &self.parts[id];
        if part.multipart || part.message_node {
            return Ok(());
        }

        let content_type = match part.content_type.as_deref() {
            Some(content_type) => content_type.to_string(),
            None if id == ROOT => "text/plain".to_string(),
            None => String::new(),
        };
        let text_type = is_text_type(&content_type);
        // Anything but "inline" is an attachment, and so is a part with no
        // disposition that is not text.
        let inline = match part
            .disposition
            .as_deref()
            .filter(|disposition| !disposition.is_empty())
        {
            Some(disposition) => disposition == "inline",
            None => text_type,
        };
        let decoder = create_transfer_decoder(match part.encoding.as_str() {
            encoding @ ("quoted-printable" | "base64") => encoding,
            _ => "binary",
        });
        let filename = part.filename.clone();
        self.parts[id].content_type = Some(content_type.clone()).filter(|value| !value.is_empty());

        if !text_type || !inline {
            let attachment_type = match filename.as_deref().filter(|filename| !filename.is_empty()) {
                Some(filename) if content_type == "application/octet-stream" => mime_type_from_filename(filename)?,
                _ => content_type,
            };
            let cid = read_message_headers(lines).content_id.map(|content_id| {
                let content_id = jsstr::trim(&content_id);
                let content_id = content_id.strip_prefix('<').unwrap_or(content_id);
                jsstr::trim(content_id.strip_suffix('>').unwrap_or(content_id)).to_string()
            });
            let (file, path) = self.new_file();
            self.attachments.push((
                id,
                ParsedAttachment {
                    path,
                    filename,
                    content_type: attachment_type,
                    size: 0,
                    cid,
                },
            ));
            self.parts[id].body = Some(PartBody {
                decoder,
                sink: BodySink::File(file),
            });
            return Ok(());
        }

        if content_type == "text/html" {
            self.has_html = true;
        } else {
            self.has_text = true;
        }
        self.parts[id].body = Some(PartBody {
            decoder,
            sink: BodySink::Text(Vec::new()),
        });
        Ok(())
    }

    fn body_bytes(&mut self, bytes: &[u8], last: bool) -> Result<(), MimeParseError> {
        let id = self.part;
        let part = &self.parts[id];
        if part.body.is_none() {
            return Ok(());
        }
        let mut content = bytes;
        let mut held: &[u8] = &[];
        if !last && part.parent.is_some() {
            // The line break before a boundary belongs to the boundary, so
            // the last one is held until the next line shows it is not
            // followed by one.
            let break_length = if self.continuing_line {
                0
            } else {
                trailing_break_length(bytes)
            };
            (content, held) = bytes.split_at(bytes.len() - break_length);
        }
        if !self.pending_break.is_empty() && (!bytes.is_empty() || last) {
            let pending = std::mem::take(&mut self.pending_break);
            self.decode(id, &pending);
        }
        if !content.is_empty() {
            self.decode(id, content);
        }
        if !held.is_empty() {
            self.pending_break = held.to_vec();
        }
        Ok(())
    }

    /// Transfer-decodes `bytes` of the part's body and delivers the result.
    fn decode(&mut self, id: PartId, bytes: &[u8]) {
        let mut decoded = std::mem::take(&mut self.scratch);
        decoded.clear();
        if let Some(body) = self.parts[id].body.as_mut() {
            body.decoder.write(bytes, &mut decoded);
        }
        self.deliver(id, &decoded);
        self.scratch = decoded;
    }

    fn deliver(&mut self, id: PartId, decoded: &[u8]) {
        let over_budget = match self.parts[id].body.as_ref().map(|body| &body.sink) {
            Some(BodySink::Text(text)) => !decoded.is_empty() && text.len() + decoded.len() > self.text_budget,
            _ => false,
        };
        if over_budget {
            self.spill_to_file(id);
        }
        match self.parts[id].body.as_mut().map(|body| &mut body.sink) {
            Some(BodySink::Text(text)) => text.extend_from_slice(decoded),
            Some(BodySink::File(file)) => file.write(decoded, &mut self.ops),
            None => {}
        }
    }

    /// Moves a text body that outgrew the budget into an attachment file.
    fn spill_to_file(&mut self, id: PartId) {
        let (mut file, path) = self.new_file();
        let part = &mut self.parts[id];
        if let Some(BodySink::Text(text)) = part.body.as_ref().map(|body| &body.sink) {
            file.write(text, &mut self.ops);
        }
        if let Some(body) = part.body.as_mut() {
            body.sink = BodySink::File(file);
        }
        let content_type = part.content_type.clone().unwrap_or_else(|| "text/plain".to_string());
        let filename = part
            .filename
            .clone()
            .unwrap_or_else(|| spill_filename(&content_type).to_string());
        self.attachments.push((
            id,
            ParsedAttachment {
                path,
                filename: Some(filename),
                content_type,
                size: 0,
                cid: None,
            },
        ));
    }

    fn finish_part(&mut self, id: PartId) {
        let Some(mut body) = self.parts[id].body.take() else {
            return;
        };
        let mut tail = Vec::new();
        body.decoder.end(&mut tail);
        if let BodySink::Text(text) = &mut body.sink {
            text.extend_from_slice(&tail);
            tail.clear();
            if text.len() > self.text_budget {
                self.parts[id].body = Some(body);
                self.spill_to_file(id);
                return self.finish_part(id);
            }
        }
        match body.sink {
            BodySink::Text(mut bytes) => {
                self.text_budget -= bytes.len();
                let part = &mut self.parts[id];
                if part.flowed {
                    bytes = decode_flowed(&bytes, part.del_sp);
                }
                part.text_content = Some(decode_body_bytes(&bytes, part.charset.as_deref()).replace("\r\n", "\n"));
            }
            BodySink::File(mut file) => {
                file.write(&tail, &mut self.ops);
                file.finish(&mut self.ops);
                if let Some((_, attachment)) = self.attachments.iter_mut().find(|(part, _)| *part == id) {
                    attachment.size = file.size;
                }
            }
        }
    }

    fn result(&mut self) -> Result<ParsedMessage, MimeParseError> {
        let (text, html) = self.text_content()?;
        let headers = std::mem::take(&mut self.message_headers);
        Ok(ParsedMessage {
            message_id: headers.message_id,
            in_reply_to: headers.in_reply_to,
            references: headers.references,
            from: headers.embedded.from,
            to: headers.embedded.to,
            cc: headers.embedded.cc,
            bcc: headers.embedded.bcc,
            reply_to: headers.reply_to,
            subject: headers.embedded.subject,
            date: headers.embedded.date,
            text,
            html,
            attachments: std::mem::take(&mut self.attachments)
                .into_iter()
                .map(|(_, attachment)| attachment)
                .collect(),
        })
    }

    /// The text and HTML bodies, each joined from the parts that carry it.
    fn text_content(&self) -> Result<(Option<String>, Option<String>), MimeParseError> {
        let mut text: Vec<String> = Vec::new();
        let mut html: Vec<String> = Vec::new();
        // Parts left to visit, each with whether a multipart/alternative encloses it.
        let mut pending = vec![(ROOT, false)];
        while let Some((id, alternative)) = pending.pop() {
            let part = &self.parts[id];
            if let Some(meta) = &part.meta_headers {
                if self.has_html {
                    html.push(embedded_headers_as_html(meta));
                }
                if self.has_text {
                    text.push(embedded_headers_as_text(meta));
                }
            }
            let content_type = part.content_type.as_deref().unwrap_or("");
            if let Some(content) = part.text_content.as_deref().filter(|content| !content.is_empty()) {
                if content_type == "text/plain" || content_type == "message/delivery-status" {
                    text.push(content.to_string());
                    if !alternative && self.has_html {
                        html.push(text_to_html(content));
                    }
                } else if content_type == "text/html" {
                    if (!alternative && self.has_text) || (id == ROOT && !self.has_text) {
                        let start = jsstr::truncate(content, HTML_TO_TEXT_INPUT_LIMIT);
                        text.push(html_to_plain_text(&start, PlainTextOptions { urls: true })?);
                    }
                    html.push(content.to_string());
                }
            }
            let child_alternative = alternative || content_type == "multipart/alternative";
            pending.extend(part.children.iter().rev().map(|&child| (child, child_alternative)));
        }
        Ok((
            (!text.is_empty()).then(|| text.join("\n")),
            (!html.is_empty()).then(|| html.join("<br/>\n")),
        ))
    }
}

/// The `cid:` reference at the start of `html[at..]`, as
/// `/\bcid:([^'"\s]{1,256})/` matches one: where it ends and the id, when
/// the id is whole (one cut through a surrogate pair names nothing).
fn cid_reference_at(html: &str, at: usize) -> Option<(usize, Option<&str>)> {
    if html[..at].chars().next_back().is_some_and(jsstr::is_word) {
        return None;
    }
    let id_start = at + "cid:".len();
    let mut end = id_start;
    let mut units = 0;
    let mut whole = true;
    for c in html[id_start..].chars() {
        if matches!(c, '\'' | '"') || jsstr::is_space(c) {
            break;
        }
        if units + c.len_utf16() > 256 {
            whole = units == 256;
            break;
        }
        units += c.len_utf16();
        end += c.len_utf8();
    }
    if units == 0 {
        return None;
    }
    // A cut through a pair still ends the match after its first half.
    let match_end = if whole {
        end
    } else {
        end + html[end..].chars().next().map_or(0, char::len_utf8)
    };
    Some((match_end, whole.then(|| &html[id_start..end])))
}

/// `/^image\/[\w]+$/i`
fn is_inline_image_type(content_type: &str) -> bool {
    let bytes = content_type.as_bytes();
    bytes.len() > 6 && bytes[..6].eq_ignore_ascii_case(b"image/") && content_type[6..].chars().all(jsstr::is_word)
}

/// Replaces `cid:` references in `html` with `data:` URLs of the image
/// attachments they name, within [`INLINE_IMAGE_BUDGET_BYTES`].
async fn inline_images(html: String, attachments: &[ParsedAttachment]) -> std::io::Result<String> {
    use base64::Engine;

    // Every reference, in order: where it is and the id it names.
    let mut references: Vec<(usize, usize, Option<&str>)> = Vec::new();
    let mut at = 0;
    while let Some(found) = html[at..].find("cid:") {
        let start = at + found;
        at = start + 1;
        if let Some((end, cid)) = cid_reference_at(&html, start) {
            references.push((start, end, cid));
            at = end;
        }
    }

    let mut wanted: Vec<(&str, &ParsedAttachment)> = Vec::new();
    for cid in references.iter().filter_map(|(_, _, cid)| *cid) {
        if wanted.iter().any(|(seen, _)| *seen == cid) {
            continue;
        }
        let image = attachments.iter().find(|attachment| {
            attachment.cid.as_deref() == Some(cid) && is_inline_image_type(&attachment.content_type)
        });
        if let Some(image) = image {
            wanted.push((cid, image));
        }
    }
    if wanted.is_empty() {
        return Ok(html);
    }

    let mut urls: HashMap<&str, String> = HashMap::new();
    let mut budget = INLINE_IMAGE_BUDGET_BYTES;
    for (cid, image) in wanted {
        if image.size > budget {
            continue;
        }
        budget -= image.size;
        let content = tokio::fs::read(&image.path).await?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(content);
        urls.insert(cid, format!("data:{};base64,{encoded}", image.content_type));
    }

    let mut inlined = String::with_capacity(html.len() + urls.values().map(String::len).sum::<usize>());
    let mut last = 0;
    for (start, end, cid) in references {
        if let Some(url) = cid.and_then(|cid| urls.get(cid)) {
            inlined.push_str(&html[last..start]);
            inlined.push_str(url);
            last = end;
        }
    }
    inlined.push_str(&html[last..]);
    Ok(inlined)
}

/// Carries out what the parser decided while reading the last chunk.
async fn run_file_ops(
    ops: &mut Vec<FileOp>,
    attachment_dir: &Path,
    files: &mut HashMap<usize, tokio::fs::File>,
    dir_ready: &mut bool,
) -> std::io::Result<()> {
    for op in ops.drain(..) {
        match op {
            FileOp::Create(index, path) => {
                if !std::mem::replace(dir_ready, true) {
                    tokio::fs::create_dir_all(attachment_dir).await?;
                }
                files.insert(index, tokio::fs::File::create(path).await?);
            }
            FileOp::Write(index, bytes) => {
                if let Some(file) = files.get_mut(&index) {
                    file.write_all(&bytes).await?;
                }
            }
            FileOp::Close(index) => {
                if let Some(mut file) = files.remove(&index) {
                    file.flush().await?;
                }
            }
        }
    }
    Ok(())
}

/// Bytes read from the source at a time.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Parses an RFC 5322 message read from `source`.
///
/// Attachments are decoded into files under `options.attachment_dir`, which
/// the caller removes once it is done with them — on failure as well, since
/// a partly written file may already be there when this fails.
///
/// ```ignore
/// let parsed = parse_message(stream, &ParseOptions { attachment_dir: dir.clone() }).await;
/// let stored = match parsed {
///     Ok(parsed) => store(parsed).await,
///     Err(error) => Err(error),
/// };
/// tokio::fs::remove_dir_all(&dir).await.ok();
/// ```
pub async fn parse_message<R>(mut source: R, options: &ParseOptions) -> Result<ParsedMessage>
where
    R: AsyncRead + Unpin,
{
    let mut parser = MimeParser::new(options);
    // Dropping these closes whatever is still open when parsing fails.
    let mut files: HashMap<usize, tokio::fs::File> = HashMap::new();
    let mut dir_ready = false;
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let read = source.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let parsed = parser.write(&chunk[..read]);
        run_file_ops(&mut parser.ops, &options.attachment_dir, &mut files, &mut dir_ready).await?;
        parsed?;
    }
    let parsed = parser.end();
    run_file_ops(&mut parser.ops, &options.attachment_dir, &mut files, &mut dir_ready).await?;
    let mut parsed = parsed?;
    if let Some(html) = parsed.html.take() {
        parsed.html = Some(inline_images(html, &parsed.attachments).await?);
    }
    Ok(parsed)
}

/// The largest time value a JavaScript `Date` holds, in either direction.
const MAX_DATE_MILLIS: i64 = 8_640_000_000_000_000;

/// Most digits of a numeral that count toward its value.
const MAX_SIGNIFICANT_DIGITS: usize = 9;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Keyword {
    /// A word that names nothing.
    Garbage,
    Month(i64),
    /// The hours `am` or `pm` add.
    AmPm(i64),
    /// A zone's offset in hours.
    Zone(i64),
    /// The `T` between an ISO date and its time.
    TimeSeparator,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum DateToken {
    Number { value: i64, length: usize },
    Symbol(char),
    Keyword { keyword: Keyword, length: usize },
    WhiteSpace,
    Unknown,
    End,
}

impl DateToken {
    fn is_number(self) -> bool {
        matches!(self, DateToken::Number { .. })
    }

    /// A number written with exactly `length` digits.
    fn fixed_number(self, length: usize) -> Option<i64> {
        match self {
            DateToken::Number { value, length: written } if written == length => Some(value),
            _ => None,
        }
    }

    fn sign(self) -> Option<i64> {
        match self {
            DateToken::Symbol('+') => Some(1),
            DateToken::Symbol('-') => Some(-1),
            _ => None,
        }
    }

    fn is_keyword_z(self) -> bool {
        self == DateToken::Keyword {
            keyword: Keyword::Zone(0),
            length: 1,
        }
    }
}

const DATE_KEYWORDS: [(&[u8], Keyword); 27] = [
    (b"jan", Keyword::Month(1)),
    (b"feb", Keyword::Month(2)),
    (b"mar", Keyword::Month(3)),
    (b"apr", Keyword::Month(4)),
    (b"may", Keyword::Month(5)),
    (b"jun", Keyword::Month(6)),
    (b"jul", Keyword::Month(7)),
    (b"aug", Keyword::Month(8)),
    (b"sep", Keyword::Month(9)),
    (b"oct", Keyword::Month(10)),
    (b"nov", Keyword::Month(11)),
    (b"dec", Keyword::Month(12)),
    (b"am", Keyword::AmPm(0)),
    (b"pm", Keyword::AmPm(12)),
    (b"ut", Keyword::Zone(0)),
    (b"utc", Keyword::Zone(0)),
    (b"z", Keyword::Zone(0)),
    (b"gmt", Keyword::Zone(0)),
    (b"cdt", Keyword::Zone(-5)),
    (b"cst", Keyword::Zone(-6)),
    (b"edt", Keyword::Zone(-4)),
    (b"est", Keyword::Zone(-5)),
    (b"mdt", Keyword::Zone(-6)),
    (b"mst", Keyword::Zone(-7)),
    (b"pdt", Keyword::Zone(-7)),
    (b"pst", Keyword::Zone(-8)),
    (b"t", Keyword::TimeSeparator),
];

/// Reads a date string token by token, one token ahead.
struct DateScanner {
    units: Vec<u16>,
    /// Whether the text has a character outside ASCII. Bun then holds it as
    /// a 16-bit string, and reads that one with a different idea of white space.
    wide: bool,
    at: usize,
    next: DateToken,
}

impl DateScanner {
    fn new(text: &str) -> DateScanner {
        let mut scanner = DateScanner {
            units: text.encode_utf16().collect(),
            wide: !text.is_ascii(),
            at: 0,
            next: DateToken::End,
        };
        scanner.next = scanner.scan();
        scanner
    }

    /// The code unit at the cursor; 0 at the end, where a NUL ends it too.
    fn unit(&self) -> u16 {
        self.units.get(self.at).copied().unwrap_or(0)
    }

    /// White space as this parser knows it: the vertical tab only in a
    /// 16-bit string, and never the line and paragraph separators, which
    /// read as letters.
    fn is_whitespace(&self, unit: u16) -> bool {
        match unit {
            0x9 | 0xa | 0xc | 0xd | 0x20 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x202f | 0x205f | 0x3000 | 0xfeff => true,
            0xb => self.wide,
            _ => false,
        }
    }

    fn scan(&mut self) -> DateToken {
        let start = self.at;
        let unit = self.unit();
        if unit == 0 {
            return DateToken::End;
        }
        let is_digit = |unit: u16| (b'0' as u16..=b'9' as u16).contains(&unit);
        if is_digit(unit) {
            // Leading zeros count toward the length, not the value.
            while self.unit() == b'0' as u16 {
                self.at += 1;
            }
            let mut value = 0;
            let mut digits = 0;
            while is_digit(self.unit()) {
                if digits < MAX_SIGNIFICANT_DIGITS {
                    value = value * 10 + (self.unit() - b'0' as u16) as i64;
                }
                digits += 1;
                self.at += 1;
            }
            return DateToken::Number {
                value,
                length: self.at - start,
            };
        }
        if let Some(symbol) = [':', '-', '+', '.', ')']
            .into_iter()
            .find(|symbol| *symbol as u16 == unit)
        {
            self.at += 1;
            return DateToken::Symbol(symbol);
        }
        if unit >= b'A' as u16 && !self.is_whitespace(unit) {
            // A word; its first three characters, lowercased, name it.
            let mut prefix = [0u16; 3];
            let mut length = 0;
            while self.unit() >= b'A' as u16 && !self.is_whitespace(self.unit()) {
                if length < 3 {
                    prefix[length] = self.unit() | 0x20;
                }
                length += 1;
                self.at += 1;
            }
            let keyword = DATE_KEYWORDS
                .iter()
                .find(|(name, keyword)| {
                    let name_matches = (0..3).all(|i| prefix[i] == name.get(i).copied().unwrap_or(0) as u16);
                    name_matches && (length <= 3 || matches!(keyword, Keyword::Month(_)))
                })
                .map_or(Keyword::Garbage, |(_, keyword)| *keyword);
            return DateToken::Keyword { keyword, length };
        }
        if self.is_whitespace(unit) {
            self.at += 1;
            return DateToken::WhiteSpace;
        }
        if unit == b'(' as u16 {
            // A parenthesized comment, nested ones included.
            let mut balance = 0;
            loop {
                match self.unit() {
                    41 => balance -= 1,
                    40 => balance += 1,
                    _ => {}
                }
                self.at += 1;
                if balance <= 0 || self.unit() == 0 {
                    break;
                }
            }
            return DateToken::Unknown;
        }
        self.at += 1;
        DateToken::Unknown
    }

    fn peek(&self) -> DateToken {
        self.next
    }

    fn next(&mut self) -> DateToken {
        let token = self.next;
        self.next = self.scan();
        token
    }

    fn skip_symbol(&mut self, symbol: char) -> bool {
        let found = self.next == DateToken::Symbol(symbol);
        if found {
            self.next = self.scan();
        }
        found
    }
}

fn is_minute_or_second(n: i64) -> bool {
    (0..=59).contains(&n)
}

#[derive(Default)]
struct DayComposer {
    components: Vec<i64>,
    named_month: Option<i64>,
    is_iso_date: bool,
}

impl DayComposer {
    fn add(&mut self, n: i64) -> bool {
        if self.components.len() == 3 {
            return false;
        }
        self.components.push(n);
        true
    }

    /// Year, month (1-12) and day.
    fn write(&self) -> Option<(i64, i64, i64)> {
        if self.components.is_empty() {
            return None;
        }
        // Day and month default to 1; so does a missing year, which then reads as 2001.
        let at = |i: usize| self.components.get(i).copied().unwrap_or(1);
        let is_day = |n: i64| (1..=31).contains(&n);
        let (mut year, month, day) = match self.named_month {
            None if self.is_iso_date || !is_day(at(0)) => (at(0), at(1), at(2)),
            None => (at(2), at(0), at(1)),
            Some(month) if !is_day(at(0)) => (at(0), month, at(1)),
            Some(month) => (at(1), month, at(0)),
        };
        if !self.is_iso_date {
            match year {
                0..=49 => year += 2000,
                50..=99 => year += 1900,
                _ => {}
            }
        }
        ((1..=12).contains(&month) && is_day(day)).then_some((year, month, day))
    }
}

#[derive(Default)]
struct TimeComposer {
    components: Vec<i64>,
    hour_offset: Option<i64>,
}

impl TimeComposer {
    fn is_expecting(&self, n: i64) -> bool {
        match self.components.len() {
            1 | 2 => is_minute_or_second(n),
            3 => (0..=999).contains(&n),
            _ => false,
        }
    }

    fn add(&mut self, n: i64) -> bool {
        if self.components.len() == 4 {
            return false;
        }
        self.components.push(n);
        true
    }

    fn add_final(&mut self, n: i64) -> bool {
        if !self.add(n) {
            return false;
        }
        self.components.resize(4, 0);
        true
    }

    /// Hour, minute, second and millisecond.
    fn write(&self) -> Option<(i64, i64, i64, i64)> {
        let at = |i: usize| self.components.get(i).copied().unwrap_or(0);
        let (mut hour, minute, second, millisecond) = (at(0), at(1), at(2), at(3));
        if let Some(offset) = self.hour_offset {
            if !(0..=12).contains(&hour) {
                return None;
            }
            hour = hour % 12 + offset;
        }
        let in_range = (0..=23).contains(&hour)
            && is_minute_or_second(minute)
            && is_minute_or_second(second)
            && (0..=999).contains(&millisecond);
        // A 24th hour is allowed if minutes, seconds, and milliseconds are 0.
        let midnight = hour == 24 && minute == 0 && second == 0 && millisecond == 0;
        (in_range || midnight).then_some((hour, minute, second, millisecond))
    }
}

#[derive(Default)]
struct TimeZoneComposer {
    sign: Option<i64>,
    hour: Option<i64>,
    minute: Option<i64>,
}

impl TimeZoneComposer {
    fn set(&mut self, offset_in_hours: i64) {
        self.sign = Some(if offset_in_hours < 0 { -1 } else { 1 });
        self.hour = Some(offset_in_hours.abs());
        self.minute = Some(0);
    }

    fn is_expecting(&self, n: i64) -> bool {
        self.hour.is_some() && self.minute.is_none() && is_minute_or_second(n)
    }

    fn is_utc(&self) -> bool {
        self.hour == Some(0) && self.minute == Some(0)
    }

    fn is_empty(&self) -> bool {
        self.hour.is_none()
    }

    /// The offset from UTC in seconds (0 for local time, which is UTC), or
    /// `None` for an offset out of range. The seconds are summed in 32 bits,
    /// so an absurd hour count wraps around before it is checked.
    fn write(&self) -> Option<i64> {
        let Some(sign) = self.sign else {
            return Some(0);
        };
        let seconds = (self.hour.unwrap_or(0) as u32)
            .wrapping_mul(3600)
            .wrapping_add((self.minute.unwrap_or(0) as u32).wrapping_mul(60));
        (seconds <= i32::MAX as u32).then_some(sign * seconds as i64)
    }
}

/// Milliseconds from the digits after a decimal point.
fn read_milliseconds(value: i64, length: usize) -> i64 {
    match length {
        1 => value * 100,
        2 => value * 10,
        3 => value,
        // More than three digits: the three most significant ones.
        _ => value / 10i64.pow(length.min(MAX_SIGNIFICANT_DIGITS) as u32 - 3),
    }
}

/// The token that ended an ECMAScript date-time string, when it ended early.
enum Es5End {
    /// The whole input was a date-time string.
    Complete,
    /// The rest is read the legacy way, starting with this token.
    Unhandled(DateToken),
    Invalid,
}

/// `[('-'|'+')yy]yyyy[-MM[-DD]][THH:mm[:ss[.sss]][Z|(+|-)hh:mm]]`
fn parse_es5_date_time(
    scanner: &mut DateScanner,
    day: &mut DayComposer,
    time: &mut TimeComposer,
    tz: &mut TimeZoneComposer,
) -> Es5End {
    if let Some(sign) = scanner.peek().sign() {
        // The sign goes back to the legacy parser when the year does not follow it.
        let sign_token = scanner.next();
        let Some(year) = scanner.peek().fixed_number(6) else {
            return Es5End::Unhandled(sign_token);
        };
        scanner.next();
        if sign < 0 && year == 0 {
            return Es5End::Unhandled(sign_token);
        }
        day.add(sign * year);
    } else if let Some(year) = scanner.peek().fixed_number(4) {
        scanner.next();
        day.add(year);
    } else {
        return Es5End::Unhandled(scanner.next());
    }
    if scanner.skip_symbol('-') {
        let Some(month) = scanner.peek().fixed_number(2).filter(|month| (1..=12).contains(month)) else {
            return Es5End::Unhandled(scanner.next());
        };
        scanner.next();
        day.add(month);
        if scanner.skip_symbol('-') {
            let Some(date) = scanner.peek().fixed_number(2).filter(|date| (1..=31).contains(date)) else {
                return Es5End::Unhandled(scanner.next());
            };
            scanner.next();
            day.add(date);
        }
    }
    if scanner.peek()
        != (DateToken::Keyword {
            keyword: Keyword::TimeSeparator,
            length: 1,
        })
    {
        if scanner.peek() != DateToken::End {
            return Es5End::Unhandled(scanner.next());
        }
    } else {
        scanner.next();
        let Some(hour) = scanner.peek().fixed_number(2).filter(|hour| (0..=24).contains(hour)) else {
            return Es5End::Invalid;
        };
        // 24:00[:00[.000]] is allowed, but no other time starting with 24.
        let zero_from_here = hour == 24;
        scanner.next();
        time.add(hour);
        if !scanner.skip_symbol(':') {
            return Es5End::Invalid;
        }
        let allowed = |n: &i64| is_minute_or_second(*n) && !(zero_from_here && *n > 0);
        let Some(minute) = scanner.peek().fixed_number(2).filter(allowed) else {
            return Es5End::Invalid;
        };
        scanner.next();
        time.add(minute);
        if scanner.skip_symbol(':') {
            let Some(second) = scanner.peek().fixed_number(2).filter(allowed) else {
                return Es5End::Invalid;
            };
            scanner.next();
            time.add(second);
            if scanner.skip_symbol('.') {
                let DateToken::Number { value, length } = scanner.peek() else {
                    return Es5End::Invalid;
                };
                if zero_from_here && value > 0 {
                    return Es5End::Invalid;
                }
                scanner.next();
                time.add(read_milliseconds(value, length));
            }
        }
        if scanner.peek().is_keyword_z() {
            scanner.next();
            tz.set(0);
        } else if let Some(sign) = scanner.peek().sign() {
            scanner.next();
            tz.sign = Some(sign);
            if let Some(hour_minute) = scanner.peek().fixed_number(4) {
                // hhmm
                scanner.next();
                let (hour, minute) = (hour_minute / 100, hour_minute % 100);
                if !(0..=23).contains(&hour) || !is_minute_or_second(minute) {
                    return Es5End::Invalid;
                }
                tz.hour = Some(hour);
                tz.minute = Some(minute);
            } else {
                // hh:mm
                let Some(hour) = scanner.peek().fixed_number(2).filter(|hour| (0..=23).contains(hour)) else {
                    return Es5End::Invalid;
                };
                scanner.next();
                tz.hour = Some(hour);
                if !scanner.skip_symbol(':') {
                    return Es5End::Invalid;
                }
                let Some(minute) = scanner.peek().fixed_number(2).filter(|n| is_minute_or_second(*n)) else {
                    return Es5End::Invalid;
                };
                scanner.next();
                tz.minute = Some(minute);
            }
        }
        if scanner.peek() != DateToken::End {
            return Es5End::Invalid;
        }
    }
    // A date with no time is UTC; a date-time with no zone is local time.
    if tz.is_empty() && time.components.is_empty() {
        tz.set(0);
    }
    day.is_iso_date = true;
    Es5End::Complete
}

/// `Date.parse(text)` as Bun reads it, which is the way V8 does: an
/// ECMAScript date-time string, or failing that the legacy format, where
/// words before the first number and text in parentheses are ignored,
/// `n:` is a time, a month name sets the month and other numbers are the
/// day, month and year in whichever order makes sense. Local time is UTC.
pub fn date_parse(text: &str) -> Option<i64> {
    let mut scanner = DateScanner::new(text);
    let mut day = DayComposer::default();
    let mut time = TimeComposer::default();
    let mut tz = TimeZoneComposer::default();

    let mut token = match parse_es5_date_time(&mut scanner, &mut day, &mut time, &mut tz) {
        Es5End::Complete => DateToken::End,
        Es5End::Unhandled(token) => token,
        Es5End::Invalid => return None,
    };
    let mut has_read_number = !day.components.is_empty();
    while token != DateToken::End {
        match token {
            DateToken::Number { value: n, .. } => {
                has_read_number = true;
                if scanner.skip_symbol(':') {
                    if scanner.skip_symbol(':') {
                        // n::
                        if !time.components.is_empty() {
                            return None;
                        }
                        time.add(n);
                        time.add(0);
                    } else {
                        // n:
                        if !time.add(n) {
                            return None;
                        }
                        if scanner.peek() == DateToken::Symbol('.') {
                            scanner.next();
                        }
                    }
                } else if scanner.skip_symbol('.') && time.is_expecting(n) {
                    time.add(n);
                    let DateToken::Number { value, length } = scanner.peek() else {
                        return None;
                    };
                    scanner.next();
                    time.add_final(read_milliseconds(value, length));
                } else if tz.is_expecting(n) {
                    tz.minute = Some(n);
                } else if time.is_expecting(n) {
                    time.add_final(n);
                    // A finished time is followed by the end, whitespace, "Z" or a sign.
                    let peek = scanner.peek();
                    let follows = peek == DateToken::End
                        || peek == DateToken::WhiteSpace
                        || peek.is_keyword_z()
                        || peek.sign().is_some();
                    if !follows {
                        return None;
                    }
                } else {
                    if !day.add(n) {
                        return None;
                    }
                    scanner.skip_symbol('-');
                }
            }
            DateToken::Keyword { keyword, .. } => match keyword {
                Keyword::AmPm(offset) if !time.components.is_empty() => time.hour_offset = Some(offset),
                Keyword::Month(month) => {
                    day.named_month = Some(month);
                    scanner.skip_symbol('-');
                }
                Keyword::Zone(offset) if has_read_number => tz.set(offset),
                _ => {
                    // Garbage words are illegal once a number has been read,
                    // and the first number has to stand apart from them.
                    if has_read_number || scanner.peek().is_number() {
                        return None;
                    }
                }
            },
            DateToken::Symbol(symbol @ ('+' | '-')) if tz.is_utc() || !time.components.is_empty() => {
                // A UTC offset, only after UTC or a time.
                tz.sign = Some(if symbol == '-' { -1 } else { 1 });
                let (mut n, mut length) = (0, 0);
                if let DateToken::Number { value, length: written } = scanner.peek() {
                    scanner.next();
                    n = value;
                    length = written;
                }
                has_read_number = true;
                if scanner.peek() == DateToken::Symbol(':') {
                    tz.hour = Some(n);
                    tz.minute = None;
                } else if length == 1 || length == 2 {
                    tz.hour = Some(n);
                    tz.minute = Some(0);
                } else if length == 3 || length == 4 {
                    tz.hour = Some(n / 100);
                    tz.minute = Some(n % 100);
                } else {
                    return None;
                }
            }
            DateToken::Symbol('+' | '-' | ')') if has_read_number => return None,
            _ => {}
        }
        token = scanner.next();
    }

    let (year, month, date) = day.write()?;
    let (hour, minute, second, millisecond) = time.write()?;
    let offset = tz.write()?;
    if year.abs() > 1_000_000 {
        return None;
    }
    // Days from the epoch to the first of the month, then the day of the month.
    let (shifted_year, shifted_month) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400);
    let day_of_year = (153 * shifted_month + 2) / 5;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468 + (date - 1);
    let ms = days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + millisecond - offset * 1000;
    (ms.abs() <= MAX_DATE_MILLIS).then_some(ms)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::mime::address::ParsedAddressValue;

    /// A message from its lines, CRLF-terminated as SMTP delivers them.
    fn message(lines: &[&str]) -> String {
        lines.iter().map(|line| format!("{line}\r\n")).collect()
    }

    struct Parsed {
        message: ParsedMessage,
        /// Holds the attachment files until the test is done with them.
        _root: tempfile::TempDir,
    }

    impl std::ops::Deref for Parsed {
        type Target = ParsedMessage;

        fn deref(&self) -> &ParsedMessage {
            &self.message
        }
    }

    /// Feeds `raw` to the parser in `size`-byte chunks.
    async fn parse_in_chunks(raw: &[u8], size: usize) -> Result<Parsed> {
        let root = tempfile::tempdir()?;
        let options = ParseOptions {
            attachment_dir: root.path().join("attachments"),
        };
        let chunks: Vec<std::io::Result<bytes::Bytes>> = raw
            .chunks(size)
            .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
            .collect();
        let source = tokio_util::io::StreamReader::new(futures_util::stream::iter(chunks));
        let message = parse_message(source, &options).await?;
        Ok(Parsed { message, _root: root })
    }

    async fn parse(raw: impl AsRef<[u8]>) -> Parsed {
        parse_in_chunks(raw.as_ref(), 64 * 1024).await.unwrap()
    }

    fn sha256(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn pseudo_random(length: usize) -> Vec<u8> {
        (0..length).map(|i| ((i * 131 + 7) % 251) as u8).collect()
    }

    fn base64_lines(bytes: &[u8]) -> String {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let mut lines = String::new();
        let mut rest = encoded.as_str();
        while rest.len() >= 76 {
            lines.push_str(&rest[..76]);
            lines.push_str("\r\n");
            rest = &rest[76..];
        }
        lines + rest
    }

    fn mixed(parts: &[&[&str]]) -> String {
        let mut lines = vec!["Content-Type: multipart/mixed; boundary=\"m\"", ""];
        for part in parts {
            lines.push("--m");
            lines.extend_from_slice(part);
        }
        lines.push("--m--");
        message(&lines)
    }

    #[tokio::test]
    async fn reads_a_single_part_message_and_keeps_its_final_line_break() {
        let parsed = parse(message(&[
            "From: Sender <sender@example.org>",
            "To: rcpt@example.com",
            "Subject: Hello",
            "Message-ID: <id-1@example.org>",
            "Date: Tue, 27 Oct 2026 10:00:00 +0900",
            "",
            "line one",
            "line two",
        ]))
        .await;
        assert_eq!(parsed.message_id.as_deref(), Some("<id-1@example.org>"));
        assert_eq!(parsed.subject.as_deref(), Some("Hello"));
        assert_eq!(
            parsed.date.map(|date| date.to_iso_string()).as_deref(),
            Some("2026-10-27T01:00:00.000Z")
        );
        assert_eq!(
            parsed.from,
            Some(ParsedAddress {
                value: vec![ParsedAddressValue {
                    address: Some("sender@example.org".into()),
                    name: "Sender".into(),
                    group: None,
                }],
                text: "\"Sender\" <sender@example.org>".into(),
            })
        );
        assert_eq!(parsed.text.as_deref(), Some("line one\nline two\n"));
        assert_eq!(parsed.html, None);
        assert!(parsed.attachments.is_empty());
    }

    #[tokio::test]
    async fn decodes_encoded_word_subjects_in_any_charset() {
        let parsed = parse(message(&[
            "Subject: =?EUC-KR?B?x9Gx2w==?= =?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?=",
            "",
            "body",
        ]))
        .await;
        assert_eq!(parsed.subject.as_deref(), Some("한글日本語"));
    }

    #[tokio::test]
    async fn keeps_the_last_of_a_repeated_single_header_and_lists_repeated_recipients() {
        let parsed = parse(message(&[
            "From: one@x.org",
            "From: two@x.org",
            "To: a@x.org",
            "To: b@x.org",
            "Subject: first",
            "Subject: last",
            "",
            "x",
        ]))
        .await;
        assert_eq!(
            parsed.from.as_ref().unwrap().value[0].address.as_deref(),
            Some("two@x.org")
        );
        assert_eq!(parsed.subject.as_deref(), Some("last"));
        let Some(OneOrMany::Many(to)) = &parsed.to else {
            panic!("repeated To headers are a list");
        };
        assert_eq!(
            to.iter().map(|to| to.text.as_str()).collect::<Vec<_>>(),
            ["a@x.org", "b@x.org"]
        );
    }

    #[tokio::test]
    async fn brackets_a_bare_message_id_and_replaces_an_unreadable_date_with_now() {
        let before = crate::js::now_ms();
        let parsed = parse(message(&["Message-ID: bare@example.org", "Date: not a date", "", "x"])).await;
        assert_eq!(parsed.message_id.as_deref(), Some("<bare@example.org>"));
        assert!(parsed.date.unwrap().timestamp_millis() >= before);
    }

    #[tokio::test]
    async fn reads_raw_utf8_header_octets_as_utf8() {
        let parsed = parse(message(&["Subject: 8비트 제목", "", "body"])).await;
        assert_eq!(parsed.subject.as_deref(), Some("8비트 제목"));
    }

    #[tokio::test]
    async fn takes_the_html_of_an_alternative_and_the_text_of_its_plain_part() {
        let parsed = parse(message(&[
            "Content-Type: multipart/alternative; boundary=\"b\"",
            "",
            "--b",
            "Content-Type: text/plain",
            "",
            "Plain",
            "--b",
            "Content-Type: text/html",
            "",
            "<p>HTML</p>",
            "--b--",
        ]))
        .await;
        assert_eq!(parsed.text.as_deref(), Some("Plain"));
        assert_eq!(parsed.html.as_deref(), Some("<p>HTML</p>"));
    }

    #[tokio::test]
    async fn renders_plain_text_for_an_html_only_message_keeping_link_and_image_urls() {
        let html = "<h1>Title</h1><p>Para <a href='https://bit.ly/3abc'>link</a> <img src='cid:a' alt='Logo'></p>";
        let parsed = parse(message(&["Content-Type: text/html", "", html])).await;
        assert_eq!(parsed.html, Some(format!("{html}\n")));
        assert_eq!(
            parsed.text.as_deref(),
            Some("TITLE\n\nPara link [https://bit.ly/3abc] Logo [cid:a]")
        );
    }

    #[tokio::test]
    async fn adds_a_plain_part_outside_the_alternative_to_the_html_with_links() {
        let parsed = parse(message(&[
            "Content-Type: multipart/mixed; boundary=\"m\"",
            "",
            "--m",
            "Content-Type: multipart/alternative; boundary=\"a\"",
            "",
            "--a",
            "Content-Type: text/plain",
            "",
            "Main",
            "--a",
            "Content-Type: text/html",
            "",
            "<p>Main</p>",
            "--a--",
            "--m",
            "Content-Type: text/plain",
            "",
            "Info: https://lists.example.org/info & more",
            "--m--",
        ]))
        .await;
        assert_eq!(
            parsed.text.as_deref(),
            Some("Main\nInfo: https://lists.example.org/info & more")
        );
        assert_eq!(
            parsed.html.as_deref(),
            Some(
                "<p>Main</p><br/>\n<p>Info: \
                 <a href=\"https://lists.example.org/info\">https://lists.example.org/info</a> &amp; more</p>"
            )
        );
    }

    #[tokio::test]
    async fn renders_a_long_plain_line_beside_an_html_part_in_linear_time() {
        let long = "a".repeat(TEXT_BODY_BUDGET_BYTES - 1024);
        let raw = message(&[
            "Content-Type: multipart/mixed; boundary=\"m\"",
            "",
            "--m",
            "Content-Type: text/html",
            "",
            "<p>hi</p>",
            "--m",
            "Content-Type: text/plain",
            "",
            &long,
            "--m--",
        ]);
        let started = Instant::now();
        let parsed = parse(raw).await;
        assert!(started.elapsed().as_millis() < 2000);
        assert!(parsed.html.as_ref().unwrap().len() > TEXT_BODY_BUDGET_BYTES - 1024);
    }

    #[tokio::test]
    async fn decodes_text_parts_in_their_declared_charset() {
        let parsed = parse(message(&[
            "Content-Type: text/plain; charset=shift_jis",
            "Content-Transfer-Encoding: base64",
            "",
            "grGC8Q==",
        ]))
        .await;
        assert_eq!(parsed.text.as_deref(), Some("こん"));
    }

    #[tokio::test]
    async fn reads_a_us_ascii_labelled_body_that_carries_8_bit_octets_as_utf8() {
        let parsed = parse(message(&["Content-Type: text/plain; charset=us-ascii", "", "café"])).await;
        assert_eq!(parsed.text.as_deref(), Some("café\n"));
    }

    #[tokio::test]
    async fn decodes_quoted_printable_soft_breaks_and_escapes() {
        let parsed = parse(message(&[
            "Content-Type: text/plain; charset=utf-8",
            "Content-Transfer-Encoding: quoted-printable",
            "",
            "caf=C3=A9 =",
            "au lait  ",
            "=3D",
        ]))
        .await;
        assert_eq!(parsed.text.as_deref(), Some("café au lait\n=\n"));
    }

    #[tokio::test]
    async fn undoes_format_flowed_with_delsp() {
        let parsed = parse(message(&[
            "Content-Type: text/plain; format=flowed; delsp=yes",
            "",
            "This is a long ",
            "flowed line.",
            "",
            "> quoted",
        ]))
        .await;
        assert_eq!(parsed.text.as_deref(), Some("This is a longflowed line.\n\n> quoted"));
    }

    #[tokio::test]
    async fn treats_lf_only_line_endings_like_crlf() {
        let parsed = parse(
            "Content-Type: multipart/alternative; boundary=\"b\"\n\n\
             --b\nContent-Type: text/plain\n\nA\nB\n--b--\n",
        )
        .await;
        assert_eq!(parsed.text.as_deref(), Some("A\nB"));
    }

    #[tokio::test]
    async fn reads_nothing_from_a_multipart_with_no_boundary() {
        let parsed = parse(message(&["Content-Type: multipart/mixed", "", "some text"])).await;
        assert_eq!(parsed.text, None);
        assert_eq!(parsed.html, None);
    }

    #[tokio::test]
    async fn shows_the_headers_of_an_inline_embedded_message() {
        let parsed = parse(message(&[
            "Content-Type: multipart/mixed; boundary=\"m\"",
            "",
            "--m",
            "Content-Type: text/plain",
            "",
            "See below.",
            "--m",
            "Content-Type: message/rfc822",
            "Content-Disposition: inline",
            "",
            "From: Inner <inner@x.org>",
            "Subject: Inner subject",
            "Date: Mon, 26 Oct 2026 09:00:00 +0000",
            "",
            "Inner body",
            "--m--",
        ]))
        .await;
        assert_eq!(
            parsed.text.as_deref(),
            Some(
                "See below.\n\nFrom: \"Inner\" <inner@x.org>\nSubject: Inner subject\n\
                 Date: Mon, 26 Oct 2026 09:00:00 GMT\n\nInner body"
            )
        );
        assert!(parsed.attachments.is_empty());
    }

    #[tokio::test]
    async fn decodes_a_delivery_status_part_as_text() {
        let parsed = parse(message(&[
            "Content-Type: multipart/report; boundary=\"r\"",
            "",
            "--r",
            "Content-Type: text/plain",
            "",
            "Failed.",
            "--r",
            "Content-Type: message/delivery-status",
            "",
            "Action: failed",
            "--r--",
        ]))
        .await;
        assert_eq!(parsed.text.as_deref(), Some("Failed.\nAction: failed"));
    }

    #[tokio::test]
    async fn writes_a_decoded_attachment_to_a_file_with_its_name_type_and_size() {
        let content = pseudo_random(100_000);
        let encoded = base64_lines(&content);
        let parsed = parse(mixed(&[
            &["Content-Type: text/plain", "", "See attached."],
            &[
                "Content-Type: application/pdf; name=\"report.pdf\"",
                "Content-Disposition: attachment; filename=\"report.pdf\"",
                "Content-Transfer-Encoding: base64",
                "",
                &encoded,
            ],
        ]))
        .await;
        assert_eq!(parsed.text.as_deref(), Some("See attached."));
        assert_eq!(parsed.attachments.len(), 1);
        let attachment = &parsed.attachments[0];
        assert_eq!(attachment.filename.as_deref(), Some("report.pdf"));
        assert_eq!(attachment.content_type, "application/pdf");
        assert_eq!(attachment.size, content.len() as u64);
        assert!(attachment.path.starts_with(parsed._root.path().join("attachments")));
        assert_eq!(sha256(&std::fs::read(&attachment.path).unwrap()), sha256(&content));
    }

    #[tokio::test]
    async fn writes_the_same_bytes_whatever_the_chunk_size() {
        let content = pseudo_random(5000);
        let encoded = base64_lines(&content);
        let raw = mixed(&[
            &["Content-Type: text/plain", "", "hi"],
            &[
                "Content-Type: application/octet-stream",
                "Content-Transfer-Encoding: base64",
                "",
                &encoded,
            ],
        ]);
        for size in [1, 2, 3, 7, 64] {
            let parsed = parse_in_chunks(raw.as_bytes(), size).await.unwrap();
            assert_eq!(parsed.text.as_deref(), Some("hi"));
            assert_eq!(
                sha256(&std::fs::read(&parsed.attachments[0].path).unwrap()),
                sha256(&content)
            );
        }
    }

    #[tokio::test]
    async fn decodes_rfc_2231_and_encoded_word_filenames() {
        let parsed = parse(mixed(&[
            &[
                "Content-Type: application/pdf",
                "Content-Disposition: attachment;",
                " filename*0*=UTF-8''%EB%B3%B4%EA%B3%A0;",
                " filename*1=\".pdf\"",
                "",
                "x",
            ],
            &[
                "Content-Type: application/octet-stream; name=\"=?UTF-8?B?7ZWc6riALmRvY3g=?=\"",
                "Content-Disposition: attachment",
                "",
                "y",
            ],
        ]))
        .await;
        let filenames: Vec<_> = parsed
            .attachments
            .iter()
            .map(|attachment| attachment.filename.as_deref())
            .collect();
        assert_eq!(filenames, [Some("보고.pdf"), Some("한글.docx")]);
    }

    #[tokio::test]
    async fn names_an_octet_stream_parts_type_after_its_filename_extension() {
        let parsed = parse(mixed(&[
            &["Content-Type: text/plain", "", "x"],
            &[
                "Content-Type: application/octet-stream",
                "Content-Disposition: attachment; filename=\"photo.jpg\"",
                "",
                "jpeg",
            ],
            &[
                "Content-Type: application/x-custom",
                "Content-Disposition: attachment",
                "",
                "raw",
            ],
        ]))
        .await;
        let types: Vec<_> = parsed
            .attachments
            .iter()
            .map(|attachment| (attachment.filename.as_deref(), attachment.content_type.as_str()))
            .collect();
        assert_eq!(
            types,
            [(Some("photo.jpg"), "image/jpeg"), (None, "application/x-custom")]
        );
    }

    #[tokio::test]
    async fn keeps_a_forwarded_message_rfc822_part_as_an_attachment() {
        let parsed = parse(mixed(&[
            &["Content-Type: text/plain", "", "Fwd"],
            &["Content-Type: message/rfc822", "", "Subject: inner", "", "inner body"],
        ]))
        .await;
        assert_eq!(parsed.text.as_deref(), Some("Fwd"));
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].content_type, "message/rfc822");
        let content = std::fs::read_to_string(&parsed.attachments[0].path).unwrap();
        assert_eq!(content, "Subject: inner\r\n\r\ninner body");
    }

    #[tokio::test]
    async fn inlines_a_cid_image_into_the_html_and_keeps_it_as_an_attachment() {
        let png = "iVBORw0KGgo=";
        let parsed = parse(message(&[
            "Content-Type: multipart/related; boundary=\"r\"",
            "",
            "--r",
            "Content-Type: text/html",
            "",
            "<img src=\"cid:logo@x\">",
            "--r",
            "Content-Type: image/png",
            "Content-ID: <logo@x>",
            "Content-Transfer-Encoding: base64",
            "",
            png,
            "--r--",
        ]))
        .await;
        assert_eq!(parsed.html, Some(format!("<img src=\"data:image/png;base64,{png}\">")));
        assert_eq!(parsed.attachments[0].cid.as_deref(), Some("logo@x"));
    }

    #[tokio::test]
    async fn leaves_a_cid_reference_in_place_past_the_inline_image_budget() {
        let image = pseudo_random(INLINE_IMAGE_BUDGET_BYTES as usize + 3);
        let encoded = base64_lines(&image);
        let parsed = parse(message(&[
            "Content-Type: multipart/related; boundary=\"r\"",
            "",
            "--r",
            "Content-Type: text/html",
            "",
            "<img src=\"cid:big@x\">",
            "--r",
            "Content-Type: image/png",
            "Content-ID: <big@x>",
            "Content-Transfer-Encoding: base64",
            "",
            &encoded,
            "--r--",
        ]))
        .await;
        assert_eq!(parsed.html.as_deref(), Some("<img src=\"cid:big@x\">"));
        assert_eq!(parsed.attachments[0].size, image.len() as u64);
    }

    #[tokio::test]
    async fn stores_a_text_body_past_the_in_memory_budget_as_an_attachment_instead() {
        let html = format!("<p>{}</p>", "x".repeat(TEXT_BODY_BUDGET_BYTES));
        let parsed = parse(format!("Content-Type: text/html\r\n\r\n{html}")).await;
        assert_eq!(parsed.html, None);
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename.as_deref(), Some("body.html"));
        assert_eq!(parsed.attachments[0].content_type, "text/html");
        assert_eq!(std::fs::read_to_string(&parsed.attachments[0].path).unwrap(), html);
    }

    #[tokio::test]
    async fn stays_linear_on_near_limit_headers_and_bodies_built_to_trip_backtracking() {
        let inputs = [
            format!("Subject: a{}b\r\n\r\nbody", "\r".repeat(900_000)),
            format!("Content-Transfer-Encoding: {}\r\n\r\nbody", "(".repeat(900_000)),
            format!("To: @@{}\r\n\r\nbody", "a!".repeat(450_000)),
            format!("Subject: a{}\r\n\r\nbody", "\r\n b".repeat(225_000)),
            format!(
                "Content-Type: text/plain; format=flowed; delsp=yes\r\n\r\n{}",
                "ab \r\n".repeat(400_000)
            ),
        ];
        for raw in inputs {
            let started = Instant::now();
            parse(raw).await;
            // Linear work here takes tens of milliseconds; quadratic takes minutes.
            assert!(started.elapsed().as_millis() < 2000);
        }
    }

    #[tokio::test]
    async fn rejects_a_message_with_too_many_parts() {
        let mut lines = vec!["Content-Type: multipart/mixed; boundary=\"m\"", ""];
        for _ in 0..=MAX_PARTS {
            lines.extend_from_slice(&["--m", "Content-Type: text/plain", "", "x"]);
        }
        lines.push("--m--");
        let failure = parse_in_chunks(message(&lines).as_bytes(), 64 * 1024)
            .await
            .err()
            .unwrap();
        assert_eq!(failure.to_string(), "Max allowed child nodes exceeded");
    }

    #[tokio::test]
    async fn rejects_a_part_whose_header_block_is_too_large() {
        let raw = message(&[&format!("X-Filler: {}", "a".repeat(1024 * 1024 + 10)), "", "body"]);
        let failure = parse_in_chunks(raw.as_bytes(), 64 * 1024).await.err().unwrap();
        assert!(failure.to_string().contains("Max header size"));
    }

    #[tokio::test]
    async fn rejects_when_the_source_fails_with_the_attachment_file_closed() {
        let root = tempfile::tempdir().unwrap();
        let options = ParseOptions {
            attachment_dir: root.path().join("attachments"),
        };
        let head = message(&[
            "Content-Type: multipart/mixed; boundary=\"m\"",
            "",
            "--m",
            "Content-Type: application/zip",
            "Content-Transfer-Encoding: base64",
            "",
        ]);
        let chunks: Vec<std::io::Result<bytes::Bytes>> = vec![
            Ok(bytes::Bytes::from(head)),
            Ok(bytes::Bytes::from(base64_lines(&pseudo_random(400_000)))),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset",
            )),
        ];
        let source = tokio_util::io::StreamReader::new(futures_util::stream::iter(chunks));
        let failure = parse_message(source, &options).await.err().unwrap();
        assert_eq!(failure.to_string(), "connection reset");
        assert_eq!(std::fs::read_dir(&options.attachment_dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&options.attachment_dir).unwrap();
        assert!(!options.attachment_dir.exists());
    }

    #[tokio::test]
    async fn reads_in_reply_to_and_references() {
        let parsed = parse(message(&[
            "Message-ID: <c@x>",
            "In-Reply-To: <b@x>",
            "References: <a@x>",
            " <b@x>",
            "Subject: Re: plan",
            "",
            "body",
        ]))
        .await;
        assert_eq!(parsed.in_reply_to.as_deref(), Some("<b@x>"));
        assert_eq!(parsed.references, Some(vec!["<a@x>".to_string(), "<b@x>".to_string()]));
    }

    #[tokio::test]
    async fn leaves_both_unset_when_the_headers_are_absent_or_empty() {
        let parsed = parse(message(&[
            "Message-ID: <c@x>",
            "In-Reply-To: ",
            "Subject: hi",
            "",
            "body",
        ]))
        .await;
        assert_eq!(parsed.in_reply_to, None);
        assert_eq!(parsed.references, None);
    }

    #[test]
    fn reads_dates_the_way_the_typescript_server_does() {
        let parse = |text: &str| {
            date_parse(text)
                .and_then(MessageDate::from_millis)
                .map(MessageDate::to_iso_string)
        };
        assert_eq!(
            parse("Tue, 27 Oct 2026 10:00:00 +0900").as_deref(),
            Some("2026-10-27T01:00:00.000Z")
        );
        assert_eq!(
            parse("2026-10-27T10:00:00.5+09:00").as_deref(),
            Some("2026-10-27T01:00:00.500Z")
        );
        assert_eq!(
            parse("Sunday, 06-Nov-94 08:49:37 GMT").as_deref(),
            Some("1994-11-06T08:49:37.000Z")
        );
        assert_eq!(
            parse("Tue Oct 27 2026 10:00:00 GMT+0900 (KST)").as_deref(),
            Some("2026-10-27T01:00:00.000Z")
        );
        assert_eq!(
            parse("27 Oct 26 10:00 EST").as_deref(),
            Some("2026-10-27T15:00:00.000Z")
        );
        assert_eq!(parse("Tue, 31 Feb 2026").as_deref(), Some("2026-03-03T00:00:00.000Z"));
        assert_eq!(parse("1").as_deref(), Some("2001-01-01T00:00:00.000Z"));
        assert_eq!(
            parse("Tue, 27 Oct 2026 23:59:60").as_deref(),
            Some("2026-10-27T23:59:00.000Z")
        );
        assert_eq!(parse("Tue, 27 Oct 2026 10:00:00 +0900 trailing"), None);
        assert_eq!(parse("Tue, 27 Oct 2026 10:00:00 JST"), None);
        assert_eq!(parse("not a date"), None);
        assert_eq!(parse(""), None);
        // The vertical tab is white space only beside a character outside ASCII.
        assert_eq!(parse("15 Oct 12:39\u{b} May"), None);
        assert_eq!(
            parse("15\u{3000}Oct 12:39\u{b} May").as_deref(),
            Some("2001-05-15T12:39:00.000Z")
        );
    }

    #[test]
    fn knows_file_types_by_extension() {
        let known = |name: &str| mime_type_from_filename(name).unwrap();
        assert_eq!(known("photo.JPG"), "image/jpeg");
        assert_eq!(known("notes.txt"), "text/plain");
        assert_eq!(known("archive.tar.gz"), "application/gzip");
        assert_eq!(known("a b.p d f"), "application/pdf");
        assert_eq!(known("pdf"), "application/pdf");
        assert_eq!(known("photo.jpg/"), "image/jpeg");
        assert_eq!(known("photo.jpg/x"), "application/octet-stream");
        assert_eq!(known("unknown.zzz"), "application/octet-stream");
        assert_eq!(known("trailing."), "application/octet-stream");
        assert!(matches!(
            mime_type_from_filename("a.b\0c"),
            Err(MimeParseError::FilenameWithNul(_))
        ));
        assert!(mime_type_from_filename(&format!("a.{}", "x".repeat(4093))).is_ok());
        assert!(matches!(
            mime_type_from_filename(&format!("a.{}", "x".repeat(4094))),
            Err(MimeParseError::FilenameTooLong(_))
        ));
    }

    #[test]
    fn takes_the_extension_of_a_path_like_node() {
        let cases = [
            ("a.b", ".b"),
            (".b", ""),
            ("a.", "."),
            ("a..b", ".b"),
            ("..", ""),
            (".", ""),
            ("a/b.c", ".c"),
            ("a.b/", ".b"),
            ("a.b/c", ""),
            ("", ""),
            ("a.b.c", ".c"),
            ("...", "."),
            (".a.b", ".b"),
            ("/", ""),
            ("a.b//", ".b"),
            ("/.b", ""),
            ("x/.b", ""),
            ("x/..", ""),
            ("a. b", ". b"),
            (" .b", ".b"),
        ];
        for (path, extension) in cases {
            assert_eq!(path_extension(path), extension, "{path:?}");
        }
    }
}
