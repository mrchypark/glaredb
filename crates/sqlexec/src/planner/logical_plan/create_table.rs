use super::*;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CreateTable {
    pub table_name: OwnedTableReference,
    pub if_not_exists: bool,
    pub schema: DFSchemaRef,
    pub source: Option<DfLogicalPlan>,
}

impl TryFrom<protogen::sqlexec::logical_plan::CreateTable> for CreateTable {
    type Error = ProtoConvError;

    fn try_from(proto: protogen::sqlexec::logical_plan::CreateTable) -> Result<Self, Self::Error> {
        let table_name = proto
            .table_name
            .ok_or(ProtoConvError::RequiredField(
                "table_name is required".to_string(),
            ))?
            .try_into()?;
        let schema = proto
            .schema
            .ok_or(ProtoConvError::RequiredField(
                "schema name is required".to_string(),
            ))?
            .try_into()?;

        if proto.source.is_some() {
            return Err(ProtoConvError::UnsupportedSerialization(
                "source is in create table not yet supported",
            ));
        }

        Ok(Self {
            table_name,
            if_not_exists: proto.if_not_exists,
            schema,
            source: None,
        })
    }
}

impl UserDefinedLogicalNodeCore for CreateTable {
    fn name(&self) -> &str {
        Self::EXTENSION_NAME
    }

    fn inputs(&self) -> Vec<&DfLogicalPlan> {
        match self.source {
            Some(ref src) => vec![src],
            None => vec![],
        }
    }

    fn schema(&self) -> &datafusion::common::DFSchemaRef {
        &self.schema
    }

    fn expressions(&self) -> Vec<datafusion::prelude::Expr> {
        vec![]
    }

    fn fmt_for_explain(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", Self::EXTENSION_NAME)
    }

    fn from_template(
        &self,
        _exprs: &[datafusion::prelude::Expr],
        _inputs: &[DfLogicalPlan],
    ) -> Self {
        self.clone()
    }
}

impl ExtensionType for CreateTable {
    const EXTENSION_NAME: &'static str = "CreateTable";

    fn try_decode_extension(extension: &LogicalPlanExtension) -> Result<Self> {
        match extension.node.as_any().downcast_ref::<Self>() {
            Some(s) => Ok(s.clone()),
            None => Err(internal!(
                "CreateTable::try_from_extension: unsupported extension",
            )),
        }
    }

    fn try_encode(&self, buf: &mut Vec<u8>, codec: &dyn LogicalExtensionCodec) -> Result<()> {
        use protogen::sqlexec::logical_plan as protogen;
        let schema = &self.schema;

        let schema: Option<datafusion_proto::protobuf::DfSchema> = schema.try_into().ok();

        let source = self.source.as_ref().map(|src| {
            LogicalPlanNode::try_from_logical_plan(src, codec)
                .map_err(|e| internal!("unable to encode source: {}", e.to_string()))
                .unwrap()
        });

        let create_table = protogen::CreateTable {
            table_name: self.table_name.clone().try_into().ok(),
            if_not_exists: self.if_not_exists,
            schema,
            source,
        };

        let extension = protogen::LogicalPlanExtensionType::CreateTable(create_table);

        let lp_extension = protogen::LogicalPlanExtension {
            inner: Some(extension),
        };

        lp_extension
            .encode(buf)
            .map_err(|e| internal!("{}", e.to_string()))?;

        Ok(())
    }
}
