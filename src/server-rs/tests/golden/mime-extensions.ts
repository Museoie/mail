/**
 * Extensions of Bun's file-type table that the generator cannot find by trying
 * every short name: those longer than four characters or with a character
 * outside a-z and 0-9. They were found once by asking `Bun.file("x." + name).type`
 * about every extension of mime-db and every name of up to five letters and digits.
 */
export const LONG_EXTENSIONS = (
  "acutc ahead appcache application atomcat atomdeleted atomsvc bcpio blorb c11amc c11amz ccxml cdbcmsg cdkey " +
  "cdmia cdmic cdmid cdmio cdmiq cdxml class coffee cryptonote dataless davmount dcurl dtshd ecelp4800 ecelp7470 " +
  "ecelp9600 emotionml fe_launch frame geojson gnumeric gramps grxml gsheet gslides heics heifs hjson inkml " +
  "install ipfix jardiff json5 jsonld jsonml karbon lasxml latex link66 list3820 listafp litcoffee lostxml maker " +
  "manifest markdown mathml mcurl meta4 metalink movie mscml mseed musicxml ngdat numbers omdoc onepkg onetmp " +
  "onetoc onetoc2 osfpvg pages pclxl pcurl pkipath pkpass portpkg provx pskcxml rcprofile rsheet scurl senmlx " +
  "sensmlx setpay setreg shtml sieve smzip stpxz stylus sv4cpio sv4crc swidtag taglet teacher teicorpus texinfo " +
  "torrent u8dsn u8hdr u8mdn u8msg unityweb ustar vcard wbxml webapp webmanifest wmlsc woff2 wspolicy x3dbz x3dvz " +
  "x_b x_t xdssc xhtml xhvml"
).split(" ");
