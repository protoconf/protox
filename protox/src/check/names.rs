use std::{
    borrow::Cow,
    collections::{hash_map, HashMap},
    fmt,
    iter::once,
};

use miette::{Diagnostic, LabeledSpan, SourceSpan};

use super::CheckError;
use crate::{
    compile::{ParsedFile, ParsedFileMap},
    index_to_i32,
    inversion_list::InversionList,
    make_absolute_name, make_name, make_span, normalize_span, parse_namespace, tag,
    types::{
        field_descriptor_proto, source_code_info::Location, DescriptorProto, EnumDescriptorProto,
        FieldDescriptorProto, FileDescriptorProto, OneofDescriptorProto, ServiceDescriptorProto,
    },
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DuplicateNameError {
    pub name: String,
    pub first: NameLocation,
    pub second: NameLocation,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NameLocation {
    Import(String),
    Root(SourceSpan),
    Unknown,
}

/// A simple map of all definitions in a proto file for checking downstream files.
#[derive(Debug, Default)]
pub(crate) struct NameMap {
    map: HashMap<String, Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    kind: DefinitionKind,
    span: Option<[i32; 4]>,
    public: bool,
    file: Option<String>,
}

#[derive(Debug, PartialEq, Clone)]
pub(crate) enum DefinitionKind {
    Package,
    Message {
        extension_numbers: InversionList,
    },
    Enum,
    EnumValue {
        parent: String,
        number: i32,
    },
    Oneof,
    Field {
        number: i32,
        ty: Option<field_descriptor_proto::Type>,
        type_name: Option<String>,
        label: Option<field_descriptor_proto::Label>,
        oneof_index: Option<i32>,
        extendee: Option<String>,
    },
    Service,
    Method,
}

struct NamePass<'a> {
    name_map: NameMap,
    scope: String,
    path: Vec<i32>,
    locations: &'a [Location],
    source: Option<&'a str>,
    errors: Vec<CheckError>,
}

impl NameMap {
    pub fn from_proto(
        file: &FileDescriptorProto,
        file_map: &ParsedFileMap,
        source: Option<&str>,
    ) -> Result<NameMap, Vec<CheckError>> {
        let mut ctx = NamePass {
            name_map: NameMap::new(),
            path: Vec::new(),
            scope: String::new(),
            locations: file
                .source_code_info
                .as_ref()
                .map(|s| s.location.as_slice())
                .unwrap_or(&[]),
            source,
            errors: Vec::new(),
        };

        ctx.add_file_descriptor_proto(file, file_map);
        debug_assert!(ctx.scope.is_empty());

        if ctx.errors.is_empty() {
            Ok(ctx.name_map)
        } else {
            Err(ctx.errors)
        }
    }

    pub fn google_descriptor() -> &'static Self {
        use once_cell::sync::Lazy;

        static INSTANCE: Lazy<NameMap> = Lazy::new(|| {
            let mut compiler =
                crate::Compiler::with_file_resolver(crate::file::GoogleFileResolver::new());
            compiler
                .add_file("google/protobuf/descriptor.proto")
                .expect("invalid descriptor.proto");
            let mut file_map = compiler.into_parsed_file_map();
            std::mem::take(&mut file_map["google/protobuf/descriptor.proto"].name_map)
        });

        &INSTANCE
    }

    fn new() -> Self {
        NameMap::default()
    }

    fn add(
        &mut self,
        name: String,
        kind: DefinitionKind,
        span: Option<[i32; 4]>,
        file: Option<&str>,
        source: Option<&str>,
        public: bool,
    ) -> Result<(), DuplicateNameError> {
        match self.map.entry(name) {
            hash_map::Entry::Vacant(entry) => {
                entry.insert(Entry {
                    file: file.map(ToOwned::to_owned),
                    kind,
                    span,
                    public,
                });
                Ok(())
            }
            hash_map::Entry::Occupied(entry) => match (kind, &entry.get().kind) {
                (DefinitionKind::Package, DefinitionKind::Package) => Ok(()),
                _ => {
                    let first = NameLocation::new(
                        entry.get().file.clone(),
                        entry.get().span.and_then(|span| make_span(span, source)),
                    );
                    let second = NameLocation::new(
                        file.map(ToOwned::to_owned),
                        span.and_then(|span| make_span(span, source)),
                    );

                    Err(DuplicateNameError {
                        name: entry.key().to_owned(),
                        first,
                        second,
                    })
                }
            },
        }
    }

    fn merge(
        &mut self,
        other: &Self,
        file: String,
        source: Option<&str>,
        public: bool,
    ) -> Result<(), CheckError> {
        for (name, entry) in &other.map {
            if entry.public {
                self.add(
                    name.clone(),
                    entry.kind.clone(),
                    entry.span,
                    Some(&file),
                    source,
                    public,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&'_ str, &'_ DefinitionKind)> {
        self.map
            .iter()
            .map(|(key, value)| (key.as_str(), &value.kind))
    }

    pub(super) fn get(&self, name: &str) -> Option<&DefinitionKind> {
        self.map.get(name).map(|e| &e.kind)
    }

    pub(super) fn resolve<'a>(
        &self,
        context: &str,
        name: &'a str,
    ) -> Option<(Cow<'a, str>, &DefinitionKind)> {
        if let Some(absolute_name) = name.strip_prefix('.') {
            self.get(absolute_name)
                .map(|def| (Cow::Borrowed(name), def))
        } else {
            let mut context = context;

            loop {
                let full_name = make_absolute_name(context, name);
                if let Some(def) = self.get(&full_name[1..]) {
                    return Some((Cow::Owned(full_name), def));
                }

                if context.is_empty() {
                    return None;
                }
                context = parse_namespace(context);
            }
        }
    }
}

impl<'a> NamePass<'a> {
    fn add_file_descriptor_proto(&mut self, file: &FileDescriptorProto, file_map: &ParsedFileMap) {
        for (index, import) in file.dependency.iter().enumerate() {
            let import_file = &file_map[import.as_str()];
            self.merge_names(
                import_file,
                file.public_dependency.contains(&index_to_i32(index)),
            );
        }

        if !file.package().is_empty() {
            for part in file.package().split('.') {
                self.add_name(part, DefinitionKind::Package, &[tag::file::PACKAGE]);
                self.enter_scope(part);
            }
        }

        self.path.extend(&[tag::file::MESSAGE_TYPE, 0]);
        for message in &file.message_type {
            self.add_descriptor_proto(message);
            self.bump_path();
        }

        self.replace_path(&[tag::file::ENUM_TYPE, 0]);
        for enu in &file.enum_type {
            self.add_enum_descriptor_proto(enu);
            self.bump_path();
        }

        self.replace_path(&[tag::file::EXTENSION, 0]);
        for extend in &file.extension {
            self.add_field_descriptor_proto(extend);
            self.bump_path();
        }

        self.replace_path(&[tag::file::SERVICE, 0]);
        for service in &file.service {
            self.add_service_descriptor_proto(service);
            self.bump_path();
        }
        self.pop_path(2);

        if !file.package().is_empty() {
            for _ in file.package().split('.') {
                self.exit_scope();
            }
        }
    }

    fn add_descriptor_proto(&mut self, message: &DescriptorProto) {
        let extension_numbers = InversionList::new(
            message
                .extension_range
                .iter()
                .map(|range| range.start()..range.end()),
        );

        self.add_name(
            message.name(),
            DefinitionKind::Message { extension_numbers },
            &[tag::message::NAME],
        );
        self.enter_scope(message.name());

        self.path.extend(&[tag::message::FIELD, 0]);
        for field in &message.field {
            self.add_field_descriptor_proto(field);
            self.bump_path();
        }

        self.replace_path(&[tag::message::ONEOF_DECL, 0]);
        for oneof in &message.oneof_decl {
            self.add_oneof_descriptor_proto(oneof);
            self.bump_path();
        }

        self.replace_path(&[tag::message::NESTED_TYPE, 0]);
        for message in &message.nested_type {
            self.add_descriptor_proto(message);
            self.bump_path();
        }

        self.replace_path(&[tag::message::ENUM_TYPE, 0]);
        for enu in &message.enum_type {
            self.add_enum_descriptor_proto(enu);
            self.bump_path();
        }

        self.replace_path(&[tag::message::EXTENSION, 0]);
        for extension in &message.extension {
            self.add_field_descriptor_proto(extension);
            self.bump_path();
        }
        self.pop_path(2);

        self.exit_scope();
    }

    fn add_field_descriptor_proto(&mut self, field: &FieldDescriptorProto) {
        self.add_name(
            field.name(),
            DefinitionKind::Field {
                ty: field_descriptor_proto::Type::from_i32(field.r#type.unwrap_or(0)),
                type_name: field.type_name.clone(),
                number: field.number(),
                label: field_descriptor_proto::Label::from_i32(field.label.unwrap_or(0)),
                oneof_index: field.oneof_index,
                extendee: field.extendee.clone(),
            },
            &[tag::field::NAME],
        );
    }

    fn add_oneof_descriptor_proto(&mut self, oneof: &OneofDescriptorProto) {
        self.add_name(oneof.name(), DefinitionKind::Oneof, &[tag::oneof::NAME]);
    }

    fn add_enum_descriptor_proto(&mut self, enum_: &EnumDescriptorProto) {
        self.add_name(enum_.name(), DefinitionKind::Enum, &[tag::enum_::NAME]);

        self.path.extend(&[tag::enum_::VALUE, 0]);
        for value in &enum_.value {
            self.add_name(
                value.name(),
                DefinitionKind::EnumValue {
                    parent: self.full_name(enum_.name()),
                    number: value.number(),
                },
                &[tag::enum_value::NAME],
            );
            self.bump_path();
        }
        self.pop_path(2);
    }

    fn add_service_descriptor_proto(&mut self, service: &ServiceDescriptorProto) {
        self.add_name(
            service.name(),
            DefinitionKind::Service,
            &[tag::service::NAME],
        );

        self.enter_scope(service.name());
        self.path.extend(&[tag::service::METHOD, 0]);
        for method in &service.method {
            self.add_name(method.name(), DefinitionKind::Method, &[tag::method::NAME]);
            self.bump_path();
        }
        self.pop_path(2);
        self.exit_scope();
    }

    fn add_name<'b>(
        &mut self,
        name: impl Into<Cow<'b, str>>,
        kind: DefinitionKind,
        path_items: &[i32],
    ) {
        let span = self.resolve_span(path_items);
        if let Err(err) =
            self.name_map
                .add(self.full_name(name), kind, span, None, self.source, true)
        {
            self.errors.push(CheckError::DuplicateName(err));
        }
    }

    fn merge_names(&mut self, file: &ParsedFile, public: bool) {
        if let Err(err) =
            self.name_map
                .merge(&file.name_map, file.name().to_owned(), self.source, public)
        {
            self.errors.push(err);
        }
    }

    fn full_name<'b>(&self, name: impl Into<Cow<'b, str>>) -> String {
        make_name(&self.scope, name.into())
    }

    fn enter_scope(&mut self, name: &str) {
        if !self.scope.is_empty() {
            self.scope.push('.');
        }
        self.scope.push_str(name);
    }

    fn exit_scope(&mut self) {
        let len = self.scope.rfind('.').unwrap_or(0);
        self.scope.truncate(len);
    }

    fn resolve_span(&mut self, path_items: &[i32]) -> Option<[i32; 4]> {
        self.path.extend(path_items);
        let span = self
            .locations
            .binary_search_by(|l| l.path.cmp(&self.path))
            .ok()
            .and_then(|index| normalize_span(&self.locations[index].span));
        self.pop_path(path_items.len());
        span
    }

    fn pop_path(&mut self, n: usize) {
        debug_assert!(self.path.len() >= n);
        self.path.truncate(self.path.len() - n);
    }

    fn bump_path(&mut self) {
        debug_assert!(self.path.len() >= 2);
        *self.path.last_mut().unwrap() += 1;
    }

    fn replace_path(&mut self, path_items: &[i32]) {
        self.pop_path(path_items.len());
        self.path.extend(path_items);
    }
}

impl NameLocation {
    fn new(file: Option<String>, span: Option<SourceSpan>) -> NameLocation {
        match (file, span) {
            (Some(file), _) => NameLocation::Import(file),
            (None, Some(span)) => NameLocation::Root(span),
            (None, None) => NameLocation::Unknown,
        }
    }
}

impl fmt::Display for DuplicateNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.first, &self.second) {
            (NameLocation::Import(first), NameLocation::Import(second)) => write!(
                f,
                "name '{}' is defined both in imported file '{}' and '{}'",
                self.name, first, second
            ),
            (NameLocation::Import(first), NameLocation::Root(_) | NameLocation::Unknown) => write!(
                f,
                "name '{}' is already defined in imported file '{}'",
                self.name, first
            ),
            _ => write!(f, "name '{}' is defined twice", self.name),
        }
    }
}

impl std::error::Error for DuplicateNameError {}

impl Diagnostic for DuplicateNameError {
    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        match (&self.first, &self.second) {
            (&NameLocation::Root(first), &NameLocation::Root(second)) => Some(Box::new(
                [
                    LabeledSpan::new_with_span(Some("first defined here…".to_owned()), first),
                    LabeledSpan::new_with_span(Some("…and defined again here".to_owned()), second),
                ]
                .into_iter(),
            )),
            (_, &NameLocation::Root(span)) | (&NameLocation::Root(span), _) => {
                Some(Box::new(once(LabeledSpan::new_with_span(
                    Some("defined here".to_owned()),
                    span,
                ))))
            }
            _ => None,
        }
    }
}
