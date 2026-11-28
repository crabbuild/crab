use crab_remote::objects::{encode_tree, object_id};
use gix_hash::ObjectId;
use gix_object::{Kind, bstr::BString, tree};

use crate::{auth::Identity, git::objects::commit_bytes};

const GITIGNORE_TEMPLATES: &[(&str, &str)] = &[
    ("Actionscript", "*.as3proj\n*.swf\nbin/\nbin-debug/\n"),
    (
        "Android",
        "*.apk\n*.aab\n*.ap_\n*.dex\n.gradle/\nbuild/\nlocal.properties\n.idea/\n",
    ),
    ("C", "*.o\n*.obj\n*.so\n*.dylib\n*.dll\n*.a\n*.lib\n*.out\n"),
    (
        "C++",
        "*.o\n*.obj\n*.so\n*.dylib\n*.dll\n*.a\n*.lib\n*.out\nbuild/\n",
    ),
    (
        "CSharp",
        "bin/\nobj/\n*.user\n*.suo\n.vs/\npackages/\nTestResults/\n",
    ),
    (
        "Clojure",
        "target/\nlib/\nclasses/\n.lein-*\n.nrepl-port\n.cpcache/\n",
    ),
    (
        "CMake",
        "CMakeCache.txt\nCMakeFiles/\ncmake_install.cmake\nMakefile\nbuild/\n",
    ),
    (
        "Dart",
        ".dart_tool/\n.packages\nbuild/\npubspec.lock\n*.g.dart\n",
    ),
    (
        "Elixir",
        "/_build/\n/cover/\n/deps/\n/doc/\n*.ez\nerl_crash.dump\n",
    ),
    (
        "Erlang",
        "*.beam\n*.dump\nebin/\n_build/\ndeps/\nerl_crash.dump\n",
    ),
    ("Go", "*.exe\n*.test\n*.out\nbin/\ncoverage.out\nvendor/\n"),
    (
        "Haskell",
        "dist/\ndist-newstyle/\n.stack-work/\n*.hi\n*.o\n",
    ),
    (
        "Java",
        "*.class\n*.jar\n*.war\n*.ear\ntarget/\nbuild/\n.gradle/\n",
    ),
    ("Julia", "*.jl.*.cov\n*.jl.cov\n*.jl.mem\ndocs/build/\n"),
    (
        "Kotlin",
        "*.class\n*.jar\nbuild/\ntarget/\n.gradle/\n.idea/\n",
    ),
    (
        "Laravel",
        "/node_modules/\n/public/hot\n/public/storage\n/vendor/\n.env\n",
    ),
    ("Lua", "*.luac\nluac.out\n*.o\n*.so\n"),
    (
        "macOS",
        ".DS_Store\n.AppleDouble\n.LSOverride\n._*\n.Spotlight-V100\n.Trashes\n",
    ),
    (
        "Maven",
        "target/\npom.xml.tag\npom.xml.releaseBackup\npom.xml.versionsBackup\n",
    ),
    (
        "Nextjs",
        ".next/\nout/\nnode_modules/\n.env*.local\nnpm-debug.log*\n",
    ),
    (
        "Node",
        "node_modules/\nnpm-debug.log*\nyarn-debug.log*\npnpm-debug.log*\ndist/\n.env\n",
    ),
    (
        "OCaml",
        "_build/\n*.install\n.merlin\n*.byte\n*.native\n_opam/\n",
    ),
    ("Perl", "blib/\n_build/\nMakefile\nMYMETA.*\n*.bak\n"),
    (
        "PHP",
        "vendor/\ncomposer.phar\n.phpunit.result.cache\n.env\n",
    ),
    (
        "Python",
        "__pycache__/\n*.py[cod]\n*$py.class\n.venv/\nvenv/\ndist/\n*.egg-info/\n",
    ),
    (
        "R",
        ".Rhistory\n.RData\n.Ruserdata\n.Rproj.user/\n*.Rcheck/\n",
    ),
    ("Ruby", "*.gem\n.bundle/\nvendor/bundle/\ncoverage/\nlog/\n"),
    ("Rust", "/target/\n**/*.rs.bk\n*.pdb\n"),
    (
        "Scala",
        "target/\nproject/target/\n.bloop/\n.metals/\n*.class\n",
    ),
    ("Swift", ".build/\n.swiftpm/\nPackages/\n*.xcodeproj/\n"),
    (
        "Terraform",
        ".terraform/\n*.tfstate\n*.tfstate.*\n*.tfvars\ncrash.log\n",
    ),
    (
        "Unity",
        "[Ll]ibrary/\n[Tt]emp/\n[Oo]bj/\n[Bb]uild/\n[Ll]ogs/\nUserSettings/\n",
    ),
    (
        "VisualStudio",
        ".vs/\n[Bb]in/\n[Oo]bj/\n*.user\n*.suo\n[Dd]ebug/\n[Rr]elease/\n",
    ),
    ("Zig", "zig-cache/\nzig-out/\n.zig-cache/\n"),
];

struct LicenseSpec {
    spdx: &'static str,
    title: &'static str,
    terms: &'static str,
    has_holder: bool,
}

const LICENSE_TEMPLATES: &[LicenseSpec] = &[
    LicenseSpec {
        spdx: "Apache-2.0",
        title: "Apache License, Version 2.0",
        terms: "Licensed under the Apache License, Version 2.0 (the \"License\");\nyou may not use this file except in compliance with the License.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "GPL-3.0-only",
        title: "GNU General Public License v3.0 only",
        terms: "This program is free software: you can redistribute it and/or modify\nit under the terms of the GNU General Public License version 3 as\npublished by the Free Software Foundation.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "MIT",
        title: "MIT License",
        terms: "Permission is hereby granted, free of charge, to any person obtaining a copy\nof this software and associated documentation files (the \"Software\"), to deal\nin the Software without restriction, including without limitation the rights\nto use, copy, modify, merge, publish, distribute, sublicense, and/or sell\ncopies of the Software, subject to standard no-warranty disclaimers.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "BSD-2-Clause",
        title: "BSD 2-Clause \"Simplified\" License",
        terms: "Redistribution and use in source and binary forms, with or without\nmodification, are permitted under the terms of the BSD 2-Clause License.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "BSD-3-Clause",
        title: "BSD 3-Clause \"New\" or \"Revised\" License",
        terms: "Redistribution and use in source and binary forms, with or without\nmodification, are permitted under the terms of the BSD 3-Clause License.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "BSL-1.0",
        title: "Boost Software License 1.0",
        terms: "Permission is hereby granted to use, reproduce, display, distribute,\nexecute, and transmit the Software under the Boost Software License 1.0.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "CC0-1.0",
        title: "Creative Commons Zero v1.0 Universal",
        terms: "Dedicated to the public domain under CC0 1.0 Universal.",
        has_holder: false,
    },
    LicenseSpec {
        spdx: "EPL-2.0",
        title: "Eclipse Public License 2.0",
        terms: "Made available under the terms of the Eclipse Public License v2.0.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "AGPL-3.0-only",
        title: "GNU Affero General Public License v3.0 only",
        terms: "This program is free software: you can redistribute it and/or modify\nit under the terms of the GNU Affero General Public License version 3\nas published by the Free Software Foundation.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "LGPL-3.0-only",
        title: "GNU Lesser General Public License v3.0 only",
        terms: "This library is free software: you can redistribute it and/or modify\nit under the terms of the GNU Lesser General Public License version 3\nas published by the Free Software Foundation.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "ISC",
        title: "ISC License",
        terms: "Permission to use, copy, modify, and/or distribute this software for any\npurpose with or without fee is hereby granted under the ISC License.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "MPL-2.0",
        title: "Mozilla Public License 2.0",
        terms: "Subject to the terms of the Mozilla Public License, v. 2.0.",
        has_holder: true,
    },
    LicenseSpec {
        spdx: "Unlicense",
        title: "The Unlicense",
        terms: "This is free and unencumbered software released into the public domain.",
        has_holder: false,
    },
];

pub(crate) fn validate_gitignore(name: &str) -> bool {
    gitignore_content(name).is_some()
}

pub(crate) fn validate_license(spdx: &str) -> bool {
    LICENSE_TEMPLATES.iter().any(|spec| spec.spdx == spdx)
}

pub(crate) fn gitignore_content(name: &str) -> Option<&'static str> {
    GITIGNORE_TEMPLATES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, content)| *content)
}

pub(crate) fn license_content(spdx: &str, year: i32, holder: &str) -> Option<String> {
    let spec = LICENSE_TEMPLATES.iter().find(|item| item.spdx == spdx)?;
    let mut text = format!("{}\nSPDX-License-Identifier: {}\n", spec.title, spec.spdx);
    if spec.has_holder {
        let author = holder.trim();
        let owner = if author.is_empty() {
            "Contributors"
        } else {
            author
        };
        text.push_str(&format!("\nCopyright (c) {year} {owner}\n"));
    }
    text.push_str(&format!(
        "\n{}\n\nFull license text: https://spdx.org/licenses/{}.html\n",
        spec.terms, spec.spdx
    ));
    Some(text)
}

pub(crate) fn readme_content(repo_name: &str, description: &str) -> String {
    let trimmed = description.trim();
    if trimmed.is_empty() {
        format!("# {repo_name}\n")
    } else {
        format!("# {repo_name}\n\n{trimmed}\n")
    }
}

/// Howard Hinnant civil-from-days algorithm in UTC.
pub(crate) fn utc_year(unix_seconds: u64) -> i32 {
    let z = (unix_seconds / 86_400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + i64::from(m <= 2)) as i32
}

pub(crate) fn build_initial_commit(
    files: &[(&str, Vec<u8>)],
    actor: &Identity,
    seconds: u64,
) -> Result<(ObjectId, Vec<(Kind, Vec<u8>)>), crab_remote::objects::Error> {
    let mut objects = Vec::with_capacity(files.len() + 2);
    let mut entries = Vec::with_capacity(files.len());
    for (name, content) in files {
        let oid = object_id(Kind::Blob, content)?;
        objects.push((Kind::Blob, content.clone()));
        entries.push(tree::Entry {
            mode: tree::EntryKind::Blob.into(),
            filename: BString::from(*name),
            oid,
        });
    }
    let tree_oid = encode_tree(entries, &mut objects)?;
    let commit = commit_bytes(tree_oid, &[], actor, "Initial commit", seconds)?;
    let commit_oid = object_id(Kind::Commit, &commit)?;
    objects.push((Kind::Commit, commit));
    Ok((commit_oid, objects))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_gitignore_and_license_templates_render_non_empty_output() {
        for (name, _) in GITIGNORE_TEMPLATES {
            assert!(validate_gitignore(name));
            assert!(!gitignore_content(name).unwrap().is_empty());
        }
        assert!(!validate_gitignore("UnknownLanguage"));

        for spec in LICENSE_TEMPLATES {
            assert!(validate_license(spec.spdx));
            let rendered = license_content(spec.spdx, 2026, "Alice").unwrap();
            assert!(rendered.contains(spec.spdx));
            if spec.has_holder {
                assert!(rendered.contains("2026 Alice"));
            }
        }
        assert!(!validate_license("Not-A-License"));
    }

    #[test]
    fn utc_year_maps_unix_timestamps() {
        assert_eq!(utc_year(0), 1970);
        assert_eq!(utc_year(1_767_225_600), 2026);
    }

    #[test]
    fn build_initial_commit_produces_root_commit_tree_and_blobs() {
        let actor = Identity {
            issuer: "urn:crab:local".into(),
            subject: "operator".into(),
            name: "Local operator".into(),
        };
        let files = [
            (
                "README.md",
                readme_content("demo", "Fast store").into_bytes(),
            ),
            (
                ".gitignore",
                gitignore_content("Rust").unwrap().as_bytes().to_vec(),
            ),
            (
                "LICENSE",
                license_content("MIT", 2026, "Alice").unwrap().into_bytes(),
            ),
        ];
        let (commit_oid, objects) = build_initial_commit(&files, &actor, 1_767_225_600).unwrap();
        assert!(!commit_oid.is_null());
        assert_eq!(objects.len(), 5);
        assert_eq!(objects.last().unwrap().0, Kind::Commit);
    }
}
