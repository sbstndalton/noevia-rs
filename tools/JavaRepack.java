// Re-pack a ZIP with java.util.zip.ZipOutputStream, the way Java-based exporters (Google Docs
// among them) write DOCX: DEFLATED entries with data descriptors and zero sizes/CRC in the local
// headers. Used by tools/gen-docx-producers.py: `java tools/JavaRepack.java in.docx out.docx`.
import java.io.*;
import java.util.*;
import java.util.zip.*;

public class JavaRepack {
    public static void main(String[] args) throws IOException {
        try (ZipFile in = new ZipFile(args[0]);
             ZipOutputStream out = new ZipOutputStream(new FileOutputStream(args[1]))) {
            for (ZipEntry e : Collections.list(in.entries())) {
                ZipEntry n = new ZipEntry(e.getName());
                n.setTime(1577836800000L);
                out.putNextEntry(n);
                try (InputStream s = in.getInputStream(e)) { s.transferTo(out); }
                out.closeEntry();
            }
        }
    }
}
