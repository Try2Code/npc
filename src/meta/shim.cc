#include "src/meta/shim.h"
#include "npc/src/meta/mod.rs.h"

#include <exiv2/exiv2.hpp>

#include <algorithm>
#include <cctype>
#include <cstring>
#include <ctime>
#include <stdexcept>
#include <string>

namespace npc {
namespace {

std::string to_std(rust::Str s) { return std::string(s.data(), s.size()); }

// Exiv2 0.27 hands back Image::AutoPtr, 0.28 Image::UniquePtr. `auto` covers
// both without a version macro.
auto open_image(const std::string &path) {
    auto image = Exiv2::ImageFactory::open(path);
    if (image.get() == nullptr) {
        throw std::runtime_error("cannot open image: " + path);
    }
    image->readMetadata();
    return image;
}

// --- safe EXIF accessors -------------------------------------------------
// Every one of these returns a default rather than throwing. A camera we have
// never seen, or a truncated file, must degrade to a null column -- never
// abort a 100k-file walk.

const Exiv2::Exifdatum *find(const Exiv2::ExifData &d, const char *key) {
    try {
        auto it = d.findKey(Exiv2::ExifKey(key));
        if (it != d.end() && it->count() > 0) return &(*it);
    } catch (...) {
    }
    return nullptr;
}

std::string str_of(const Exiv2::ExifData &d, const char *key) {
    if (auto *v = find(d, key)) {
        try {
            std::string s = v->toString();
            // trim
            size_t b = s.find_first_not_of(" \t\r\n");
            size_t e = s.find_last_not_of(" \t\r\n");
            if (b == std::string::npos) return "";
            return s.substr(b, e - b + 1);
        } catch (...) {
        }
    }
    return "";
}

// Interpreted form -- for lens, the raw value is a Nikon ID number and the
// pretty-printed form is the human name.
std::string pretty_of(const Exiv2::ExifData &d, const char *key) {
    if (auto *v = find(d, key)) {
        try {
            std::ostringstream os;
            v->write(os, &d);
            return os.str();
        } catch (...) {
        }
    }
    return "";
}

uint32_t u32_of(const Exiv2::ExifData &d, const char *key) {
    if (auto *v = find(d, key)) {
        try {
            long l = v->toLong();
            if (l > 0) return static_cast<uint32_t>(l);
        } catch (...) {
        }
    }
    return 0;
}

double f64_of(const Exiv2::ExifData &d, const char *key) {
    if (auto *v = find(d, key)) {
        try {
            return v->toFloat();
        } catch (...) {
        }
    }
    return 0.0;
}

// EXIF DateTimeOriginal is "YYYY:MM:DD HH:MM:SS" with no timezone. We store it
// as if UTC (timegm) so that date grouping and range queries are stable and
// reversible; this is a labelling convention, not a claim about the timezone.
int64_t parse_exif_time(const std::string &s) {
    if (s.size() < 19) return 0;
    std::tm tm{};
    if (sscanf(s.c_str(), "%4d:%2d:%2d %2d:%2d:%2d", &tm.tm_year, &tm.tm_mon,
               &tm.tm_mday, &tm.tm_hour, &tm.tm_min, &tm.tm_sec) != 6) {
        return 0;
    }
    if (tm.tm_year < 1900) return 0;
    tm.tm_year -= 1900;
    tm.tm_mon -= 1;
    time_t t = timegm(&tm);
    return t == static_cast<time_t>(-1) ? 0 : static_cast<int64_t>(t);
}

// --- XMP helpers ---------------------------------------------------------

// dc:title and dc:description are XMP language alternatives (rdf:Alt keyed by
// xml:lang), so a plain toString() yields `lang="x-default" Some Title`. Pull
// the x-default branch out properly -- stripping the prefix textually would
// corrupt any title that genuinely starts with `lang=`.
std::string xmp_str(const Exiv2::XmpData &d, const char *key) {
    try {
        auto it = d.findKey(Exiv2::XmpKey(key));
        if (it == d.end()) return "";

        const Exiv2::Value &v = it->value();
        if (v.typeId() == Exiv2::langAlt) {
            const auto &la = static_cast<const Exiv2::LangAltValue &>(v);
            auto found = la.value_.find("x-default");
            if (found != la.value_.end()) return found->second;
            if (!la.value_.empty()) return la.value_.begin()->second;
            return "";
        }
        return it->toString();
    } catch (...) {
    }
    return "";
}

// Counterpart for writing: store as a proper x-default language alternative so
// Lightroom, digiKam and darktable read it as a title rather than a raw string.
void set_lang_alt(Exiv2::XmpData &d, const char *key, const std::string &text) {
    Exiv2::Value::AutoPtr v = Exiv2::Value::create(Exiv2::langAlt);
    v->read("lang=x-default " + text);
    d.add(Exiv2::XmpKey(key), v.get());
}

int32_t xmp_int(const Exiv2::XmpData &d, const char *key, int32_t dflt) {
    try {
        auto it = d.findKey(Exiv2::XmpKey(key));
        if (it != d.end()) return static_cast<int32_t>(it->toLong());
    } catch (...) {
    }
    return dflt;
}

void erase_key(Exiv2::XmpData &d, const char *key) {
    try {
        auto it = d.findKey(Exiv2::XmpKey(key));
        while (it != d.end()) {
            d.erase(it);
            it = d.findKey(Exiv2::XmpKey(key));
        }
    } catch (...) {
    }
}

} // namespace

// -------------------------------------------------------------------------

void xmp_init() { Exiv2::XmpParser::initialize(); }
void xmp_shutdown() { Exiv2::XmpParser::terminate(); }

FileMeta read_meta(rust::Str path_) {
    const std::string path = to_std(path_);
    auto image = open_image(path); // read-only

    FileMeta m{};
    m.width = static_cast<uint32_t>(image->pixelWidth());
    m.height = static_cast<uint32_t>(image->pixelHeight());

    const Exiv2::ExifData &d = image->exifData();

    std::string dto = str_of(d, "Exif.Photo.DateTimeOriginal");
    if (dto.empty()) dto = str_of(d, "Exif.Image.DateTime");
    m.capture_time = parse_exif_time(dto);

    std::string make = str_of(d, "Exif.Image.Make");
    std::string model = str_of(d, "Exif.Image.Model");
    if (!make.empty() && model.rfind(make, 0) != 0) {
        m.camera = rust::String(make + " " + model);
    } else {
        m.camera = rust::String(model);
    }

    std::string lens = pretty_of(d, "Exif.NikonLd3.LensIDNumber");
    if (lens.empty()) lens = str_of(d, "Exif.Photo.LensModel");
    if (lens.empty()) lens = pretty_of(d, "Exif.NikonLd2.LensIDNumber");
    m.lens = rust::String(lens);

    m.iso = u32_of(d, "Exif.Photo.ISOSpeedRatings");
    if (m.iso == 0) m.iso = u32_of(d, "Exif.Nikon3.ISOSpeed");
    m.fnumber = f64_of(d, "Exif.Photo.FNumber");
    m.exposure = f64_of(d, "Exif.Photo.ExposureTime");
    m.focal = f64_of(d, "Exif.Photo.FocalLength");

    // Exposure compensation is a signed rational, and 0 EV is both the most
    // common real value and what a missing tag would look like -- so the
    // presence flag is not redundant.
    if (auto *v = find(d, "Exif.Photo.ExposureBiasValue")) {
        try {
            m.exposure_bias = v->toFloat();
            m.has_bias = true;
        } catch (...) {
            m.has_bias = false;
        }
    } else {
        m.has_bias = false;
    }

    uint32_t o = u32_of(d, "Exif.Image.Orientation");
    m.orientation = static_cast<uint16_t>(o == 0 ? 1 : o);

    std::string serial = str_of(d, "Exif.Nikon3.SerialNumber");
    if (serial.empty()) serial = str_of(d, "Exif.Photo.BodySerialNumber");
    m.serial = rust::String(serial);

    m.shutter_count = u32_of(d, "Exif.Nikon3.ShutterCount");

    // In-camera rating, if the body wrote one. -1 means unset.
    if (auto *v = find(d, "Exif.Image.Rating")) {
        try {
            m.rating = static_cast<int32_t>(v->toLong());
        } catch (...) {
            m.rating = -1;
        }
    } else {
        m.rating = -1;
    }

    // Keywords another application embedded directly in the file -- Shotwell
    // writes both of the following, other tools write one or the other, so
    // both are read and unioned case-insensitively. This is distinct from a
    // sidecar's tags (Xmp.dc.subject in a .xmp, read by read_sidecar): these
    // came from the original, before this tool ever saw the library.
    auto add_tag = [&m](const std::string &t) {
        if (t.empty()) return;
        for (const auto &existing : m.tags) {
            // Convert once into a named local. Calling `.begin()`/`.end()` on
            // two independently-constructed `std::string(existing)` temporaries
            // (the original form of this code) compared iterators from two
            // different objects instead of one, and silently never matched --
            // confirmed by instrumenting it directly, not by inspection.
            const std::string e(existing);
            if (std::equal(t.begin(), t.end(), e.begin(), e.end(), [](char a, char b) {
                    return std::tolower(static_cast<unsigned char>(a)) ==
                           std::tolower(static_cast<unsigned char>(b));
                })) {
                return;
            }
        }
        m.tags.push_back(rust::String(t));
    };

    try {
        const Exiv2::XmpData &xd = image->xmpData();

        // Not every tool that rates a file also writes the plain EXIF tag --
        // this one was found writing only Xmp.xmp.Rating (no Exif.Image.Rating
        // at all), which silently dropped its rating until this fallback was
        // added. Same key `read_sidecar` already reads from a real sidecar.
        if (m.rating < 0) {
            m.rating = xmp_int(xd, "Xmp.xmp.Rating", -1);
        }

        auto it = xd.findKey(Exiv2::XmpKey("Xmp.dc.subject"));
        if (it != xd.end()) {
            for (long i = 0; i < it->count(); ++i) {
                add_tag(it->toString(i));
            }
        }
    } catch (...) {
    }

    try {
        const Exiv2::IptcData &id = image->iptcData();
        for (const auto &datum : id) {
            if (datum.key() == "Iptc.Application2.Keywords") {
                add_tag(datum.toString());
            }
        }
    } catch (...) {
    }

    m.ok = true;
    return m;
}

SidecarDoc read_sidecar(rust::Str xmp_path_) {
    const std::string path = to_std(xmp_path_);

    SidecarDoc doc{};
    doc.rating = -1;
    doc.orientation = 0;

    std::unique_ptr<Exiv2::Image> probe;
    try {
        auto image = Exiv2::ImageFactory::open(path);
        if (image.get() == nullptr) return doc; // exists=false
        image->readMetadata();

        const Exiv2::XmpData &d = image->xmpData();
        doc.exists = true;

        try {
            auto it = d.findKey(Exiv2::XmpKey("Xmp.dc.subject"));
            if (it != d.end()) {
                for (long i = 0; i < it->count(); ++i) {
                    std::string t = it->toString(i);
                    if (!t.empty()) doc.tags.push_back(rust::String(t));
                }
            }
        } catch (...) {
        }

        doc.rating = xmp_int(d, "Xmp.xmp.Rating", -1);
        doc.label = rust::String(xmp_str(d, "Xmp.xmp.Label"));
        doc.title = rust::String(xmp_str(d, "Xmp.dc.title"));
        doc.description = rust::String(xmp_str(d, "Xmp.dc.description"));
        doc.orientation = static_cast<uint16_t>(xmp_int(d, "Xmp.tiff.Orientation", 0));
    } catch (const Exiv2::Error &) {
        // Missing or unreadable sidecar is not an error: it means "no metadata
        // yet". Callers distinguish via doc.exists.
        return doc;
    }

    return doc;
}

void write_sidecar(rust::Str xmp_path_, const SidecarDoc &doc) {
    const std::string path = to_std(xmp_path_);

    // THE GUARD. This is what makes the invariant checkable rather than
    // trusted: no argument that is not a sidecar can reach writeMetadata().
    if (path.size() < 4 ||
        path.compare(path.size() - 4, 4, ".xmp") != 0) {
        throw std::runtime_error(
            "refusing to write to a non-sidecar path: " + path +
            " (this tool never modifies original image files)");
    }

    // Read what is already there, so foreign properties survive. Never build a
    // fresh XmpData and never call clear() -- that is exactly how darktable's
    // and Lightroom's data gets silently destroyed.
    Exiv2::XmpData xmp;
    try {
        auto existing = Exiv2::ImageFactory::open(path);
        if (existing.get() != nullptr) {
            existing->readMetadata();
            xmp = existing->xmpData();
        }
    } catch (const Exiv2::Error &) {
        // No existing sidecar; start from empty.
    }

    // Modify only our keys, in place.
    erase_key(xmp, "Xmp.dc.subject");
    if (!doc.tags.empty()) {
        Exiv2::Value::AutoPtr v = Exiv2::Value::create(Exiv2::xmpBag);
        for (const auto &t : doc.tags) {
            v->read(std::string(t));
        }
        xmp.add(Exiv2::XmpKey("Xmp.dc.subject"), v.get());
    }

    erase_key(xmp, "Xmp.xmp.Rating");
    if (doc.rating >= 0) xmp["Xmp.xmp.Rating"] = static_cast<int32_t>(doc.rating);

    erase_key(xmp, "Xmp.xmp.Label");
    if (!doc.label.empty()) xmp["Xmp.xmp.Label"] = std::string(doc.label);

    erase_key(xmp, "Xmp.dc.title");
    if (!doc.title.empty()) set_lang_alt(xmp, "Xmp.dc.title", std::string(doc.title));

    erase_key(xmp, "Xmp.dc.description");
    if (!doc.description.empty())
        set_lang_alt(xmp, "Xmp.dc.description", std::string(doc.description));

    erase_key(xmp, "Xmp.tiff.Orientation");
    if (doc.orientation > 0)
        xmp["Xmp.tiff.Orientation"] = static_cast<int32_t>(doc.orientation);

    auto out = Exiv2::ImageFactory::create(Exiv2::ImageType::xmp, path);
    out->setXmpData(xmp);
    out->writeMetadata(); // <-- one of exactly two writers; see shim.h
}

void stamp_export(rust::Str dest_path_, const SidecarDoc &doc) {
    const std::string path = to_std(dest_path_);

    auto image = Exiv2::ImageFactory::open(path);
    if (image.get() == nullptr) {
        throw std::runtime_error("cannot open export for stamping: " + path);
    }
    image->readMetadata();

    Exiv2::XmpData xmp = image->xmpData();
    erase_key(xmp, "Xmp.dc.subject");
    if (!doc.tags.empty()) {
        Exiv2::Value::AutoPtr v = Exiv2::Value::create(Exiv2::xmpBag);
        for (const auto &t : doc.tags) v->read(std::string(t));
        xmp.add(Exiv2::XmpKey("Xmp.dc.subject"), v.get());
    }
    erase_key(xmp, "Xmp.xmp.Rating");
    if (doc.rating >= 0) xmp["Xmp.xmp.Rating"] = static_cast<int32_t>(doc.rating);
    erase_key(xmp, "Xmp.dc.title");
    if (!doc.title.empty()) set_lang_alt(xmp, "Xmp.dc.title", std::string(doc.title));
    erase_key(xmp, "Xmp.dc.description");
    if (!doc.description.empty())
        set_lang_alt(xmp, "Xmp.dc.description", std::string(doc.description));

    image->setXmpData(xmp);
    image->writeMetadata();
}

PreviewBlob extract_preview(rust::Str path_, uint32_t min_dim) {
    const std::string path = to_std(path_);
    auto image = open_image(path); // read-only

    PreviewBlob blob{};

    Exiv2::PreviewManager pm(*image);
    Exiv2::PreviewPropertiesList props = pm.getPreviewProperties();
    if (props.empty()) {
        blob.ok = false;
        blob.error = rust::String("no embedded preview");
        return blob;
    }

    // Exiv2 returns these ascending by size. Take the smallest that still meets
    // the requested dimension -- on a D750 NEF that means a 140 KB read for a
    // grid thumbnail rather than the 1.75 MB full-resolution preview.
    const Exiv2::PreviewProperties *chosen = nullptr;
    for (const auto &p : props) {
        if (std::max(p.width_, p.height_) >= min_dim) {
            chosen = &p;
            break;
        }
    }
    if (chosen == nullptr) chosen = &props.back(); // nothing big enough: best available

    Exiv2::PreviewImage preview = pm.getPreviewImage(*chosen);
    const Exiv2::byte *data = preview.pData();
    size_t len = preview.size();

    blob.data.reserve(len);
    for (size_t i = 0; i < len; ++i) blob.data.push_back(data[i]);
    blob.width = static_cast<uint32_t>(preview.width());
    blob.height = static_cast<uint32_t>(preview.height());
    blob.mime = rust::String(preview.mimeType());
    blob.ok = true;
    return blob;
}

} // namespace npc
